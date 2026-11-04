//! Guards: the cash policy's response to obligations cash cannot meet, the mandate's constraints, and
//! liquidation at the horizon.

use super::*;

impl Ledger<'_> {
    /// Meets a cash need by the book's responses, in order. What they cannot cover is an
    /// economic failure (kept in the results, unless set to fail the run).
    pub(super) fn check_cash(&mut self, k: usize, t: Ts, acc: &mut Acc) -> Result<()> {
        for r in self.cfg.responses.clone() {
            match r {
                Response::Convert => self.convert(k, t),
                Response::Draw => self.draw(k, t),
                Response::Sell { cost_bp } => self.sell(k, t, cost_bp, acc),
            }
            // Cash a response raised pays what is due and still owed.
            self.pay_overdue(k, t);
        }
        let floor = -self.borrowable();
        let scale = self.st.out.nav.last().copied().unwrap_or(self.cfg.initial_cash).abs().max(1.0);
        let mut short = 0.0;
        for s in 0..self.slots.len() {
            let c = self.st.cash[s].to_f64();
            let lim = if s == 0 { floor } else { 0.0 };
            if c < lim - TINY * scale {
                short += (lim - c) * self.st.fx[s];
            }
            short += self.unmet(k, s) * self.st.fx[s];
        }
        if short > 0.0 {
            self.st.out.shortfall = self.st.out.shortfall.max(short);
            if breach_of(&self.cfg.cash) == Breach::Fail {
                return Err(Error::Invalid(format!("at {t} the book owes {short:.2} more than its cash can pay")));
            }
            if self.st.out.failed_at.is_none() {
                self.st.out.failed_at = Some(t);
                self.log(t, EntryKind::Failure, EntityId::NONE, 0, 0.0, -short, Cause::Book, Some("an obligation the cash cannot meet".into()));
            }
        }
        Ok(())
    }

    pub(super) fn check_constraints(&mut self, k: usize, t: Ts, nav: f64) -> Result<()> {
        if self.cfg.constraints.is_empty() {
            return Ok(());
        }
        let view = View::new(self.ws, t, t);
        let constraints = self.cfg.constraints.clone();
        for (ci, c) in constraints.iter().enumerate() {
            let mut sums: HashMap<u64, f64> = HashMap::new();
            for &i in &self.st.held {
                let e = self.records.entities()[i];
                let key = match &c.scope {
                    ConstraintScope::Book => 0,
                    ConstraintScope::Contracts(es) => {
                        if !es.contains(&e) {
                            continue;
                        }
                        0
                    }
                    ConstraintScope::PerGroup { .. } => match read(self.groups[ci], e, t, t) {
                        Some(g) => g.to_bits(),
                        None => continue,
                    },
                };
                let m = c.measure.per_unit(self.records, i, k, &view).unwrap_or(f64::NAN);
                *sums.entry(key).or_default() += self.st.q[i] * m * self.st.fx[self.row_slot[i]];
            }
            let mut keys: Vec<u64> = sums.keys().copied().collect();
            keys.sort_unstable();
            for key in keys {
                let x = if c.relative { sums[&key] / nav } else { sums[&key] };
                let excess = c.max.map_or(0.0, |hi| (x - hi).max(0.0)) + c.min.map_or(0.0, |lo| (lo - x).max(0.0));
                let tol = 1e-9 * c.max.or(c.min).map_or(1.0, |b| b.abs().max(1.0));
                if excess > tol || x.is_nan() {
                    let r = &mut self.st.out.breaches[ci];
                    r.count += 1;
                    r.first.get_or_insert(t);
                    r.worst = r.worst.max(excess);
                    if !c.hard {
                        r.penalty += c.penalty.unwrap_or(0.0) * excess;
                    }
                    let note = format!("{} at {x}", c.name);
                    self.log(t, EntryKind::Breach, EntityId::NONE, 0, 0.0, excess, Cause::Book, Some(note));
                    if c.hard && c.fail {
                        return Err(Error::Invalid(format!("hard constraint {} breached at {t}: {x}", c.name)));
                    }
                }
            }
        }
        Ok(())
    }

    /// Sells everything at the horizon through the execution model, once.
    pub(super) fn liquidate(&mut self, k: usize, t: Ts, nav: f64) {
        let mut batch = Vec::new();
        for &i in &self.st.held {
            let q = self.st.q[i];
            batch.push(WorkingOrder {
                id: u64::MAX - i as u64,
                entity: self.records.entities()[i],
                row: i,
                remaining: -q,
                requested: -q,
                filled: 0.0,
                paid: 0.0,
                submitted: t,
                submitted_k: k,
                workable_k: k,
                expires_k: k,
                attempts: 0,
                policy: usize::MAX,
                basket: u64::MAX,
                decision_value: self.value(i, k),
            });
        }
        let values: Vec<f64> = batch.iter().map(|o| self.value(o.row, k)).collect();
        let fills = self.exec.work(t, &batch, &values);
        let mut costs = 0.0;
        let mut residual = 0.0;
        for ((o, v), fill) in batch.iter().zip(&values).zip(&fills) {
            let s = self.row_slot[o.row];
            let x = self.st.fx[s];
            let f = if fill.qty.signum() == o.remaining.signum() { fill.qty.abs().min(o.remaining.abs()).copysign(o.remaining) } else { 0.0 };
            costs += f * (fill.price - v) * x;
            residual += (o.remaining - f) * -v * x;
            self.log(t, EntryKind::Liquidation, o.entity, s, f, -f * fill.price, Cause::Horizon, None);
        }
        self.st.out.liquidation_nav = Some(nav - costs);
        self.st.out.liquidation_residual = residual;
    }
}
