//! Financing: recalls and agreements (secured borrowing and annexes: valuation, haircuts, calls and
//! returns), and the responses to a shortfall (conversion, drawing, selling).

use super::*;

impl Ledger<'_> {
    /// A borrowed security recalled below the short position is bought back at the next
    /// execution.
    pub(super) fn recalls(&mut self, k: usize, t: Ts) {
        if self.available.is_none() {
            return;
        }
        let held = self.st.held.clone();
        for i in held {
            let q = self.st.q[i];
            if q >= 0.0 || !self.records.needs_borrow[i] {
                continue;
            }
            let e = self.records.entities()[i];
            let Some(avail) = read(self.available, e, t, t) else { continue };
            let excess = -q - avail.max(0.0);
            if excess > TINY && !self.st.working.iter().any(|o| o.row == i && o.remaining > 0.0) {
                let id = self.log(t, EntryKind::Recall, e, self.row_slot[i], excess, 0.0, Cause::Book, None);
                self.st.out.orders.submitted += 1;
                let v = self.value(i, k);
                self.st.working.push(WorkingOrder {
                    id,
                    entity: e,
                    row: i,
                    remaining: excess,
                    requested: excess,
                    filled: 0.0,
                    paid: 0.0,
                    submitted: t,
                    submitted_k: k,
                    workable_k: k + 1,
                    expires_k: k + self.cfg.time_in_force,
                    attempts: 0,
                    policy: usize::MAX,
                    basket: u64::MAX,
                    decision_value: v,
                });
            }
        }
    }

    /// Secured agreements' borrowing bases, margin calls and maturities, and annexes' margin. A
    /// call due now is paid from cash; a shortfall is then a cash need for the responses.
    pub(super) fn agreements(&mut self, k: usize, t: Ts) {
        if self.fin.is_empty() {
            return;
        }
        let n = self.records.entities().len();
        let mut pledged = vec![(0.0, usize::MAX); n];
        for j in 0..self.fin.len() {
            match self.cfg.financing[j].kind.clone() {
                AgreementKind::Secured { basis, concentration, limit, maturity, cure, sweep, margin, rounding, .. } => {
                    let mut advanced = Vec::new();
                    let mut value = 0.0;
                    for &(i, col, fixed) in &self.fin[j].rows {
                        let free = self.st.q[i] - pledged[i].0;
                        if free <= TINY {
                            continue;
                        }
                        let adv = if col.is_some() { read(col, self.records.entities()[i], t, t).unwrap_or(0.0) } else { fixed };
                        let unit = match basis {
                            Basis::Market => self.value(i, k),
                            Basis::Par => self.records.face[self.records.at(i, k)],
                        } * self.st.fx[self.row_slot[i]];
                        if adv.is_nan() || adv <= 0.0 || !unit.is_finite() || unit <= 0.0 {
                            continue;
                        }
                        // Pledged here, so no later agreement can count it.
                        pledged[i] = (pledged[i].0 + free, if pledged[i].1 == usize::MAX { j } else { pledged[i].1 });
                        value += free * unit;
                        advanced.push(free * unit * adv.min(1.0));
                    }
                    let eligible: f64 = advanced.iter().sum();
                    // An excess concentration: what a holding advances above its share of the
                    // aggregate eligible collateral does not count.
                    let base = concentration.map_or(eligible, |c| advanced.iter().map(|v| v.min(c * eligible)).sum());
                    let matured = maturity.is_some_and(|m| t >= m);
                    // The lender assesses margin at its margin times (or at every step), and at
                    // maturity, when the whole balance is due.
                    let assess = matured || margin.as_ref().is_none_or(|m| m.binary_search(&t).is_ok());
                    let capacity = if matured { 0.0 } else { base.min(limit) };
                    let st = &mut self.st.agreements[j];
                    st.capacity = rounding.map_or(capacity, |r| r.round(capacity));
                    st.value = value;
                    st.called = 0.0;
                    st.paid = 0.0;
                    st.matured = matured;
                    if assess {
                        let excess = st.balance - st.capacity;
                        if excess > TINY * st.balance.max(1.0) {
                            if st.call <= 0.0 {
                                st.call_due = if matured { k } else { k + cure };
                            }
                            st.call = excess;
                            st.called = excess;
                        } else {
                            st.call = 0.0;
                        }
                    }
                    if self.st.agreements[j].call > 0.0 && k >= self.st.agreements[j].call_due {
                        self.pay_call(j, t);
                    }
                    if sweep {
                        let x = self.st.cash[0].to_f64().min(self.st.agreements[j].balance);
                        if x > TINY {
                            self.st.agreements[j].balance -= x;
                            self.st.cash[0] -= Cash::from_f64(x);
                            self.log(t, EntryKind::Financing, EntityId::NONE, 0, 0.0, -x, Cause::Agreement(j), Some("repaid from free cash".into()));
                        }
                    }
                }
                AgreementKind::Csa { threshold, minimum, every, .. } => {
                    let mtm: f64 =
                        self.fin[j].set.iter().map(|&i| self.st.q[i] * self.value(i, k) * self.st.fx[self.row_slot[i]]).filter(|x| x.is_finite()).sum();
                    let required = if mtm < -threshold {
                        -mtm - threshold
                    } else if mtm > threshold {
                        -(mtm - threshold)
                    } else {
                        0.0
                    };
                    let st = &mut self.st.agreements[j];
                    st.capacity = mtm;
                    st.value = required;
                    let delta = required - st.balance;
                    if k.is_multiple_of(every) && delta.abs() >= minimum.max(TINY) {
                        st.balance += delta;
                        self.st.cash[0] -= Cash::from_f64(delta);
                        let why = if delta > 0.0 { "collateral posted" } else { "collateral returned or received" };
                        self.log(t, EntryKind::Financing, EntityId::NONE, 0, 0.0, -delta, Cause::Agreement(j), Some(why.into()));
                    }
                }
            }
        }
        self.st.pledged = pledged;
    }

    /// Base cash needed: what is due and unpaid in the base currency, and anything below the
    /// floor.
    pub(super) fn need(&self, k: usize) -> f64 {
        self.unmet(k, 0) + (-self.borrowable() - self.st.cash[0].to_f64()).max(0.0)
    }

    /// Exchanges base cash for other currencies short of cash for what is due in them.
    /// Covers what one currency owes with another's cash, at the FX rate and the conversion
    /// spread (a cost): another currency's shortfall from base cash, then base's own shortfall
    /// from the other currencies' spare cash (a sale's proceeds abroad paying a redemption at home).
    pub(super) fn convert(&mut self, k: usize, t: Ts, acc: &mut Acc) {
        if !self.cfg.conversion.auto {
            return;
        }
        let spread = self.cfg.conversion.spread_bp * 1e-4;
        for s in 1..self.slots.len() {
            let short = self.unmet(k, s) + (-self.st.cash[s].to_f64()).max(0.0);
            if short > TINY {
                let (x, rate) = (self.st.fx[s], self.st.fx[s] * (1.0 + spread));
                let base = self.st.cash[0].to_f64().max(0.0) + self.borrowable();
                let got = short.min(base / rate);
                if got > 0.0 {
                    self.st.cash[0] -= Cash::from_f64(got * rate);
                    self.st.cash[s] += Cash::from_f64(got);
                    acc.costs += got * (rate - x);
                    self.log(t, EntryKind::Conversion, self.slots[s], s, got, got * rate, Cause::Book, Some("covering an obligation".into()));
                }
            }
        }
        let mut short = self.unmet(k, 0) + (-(self.st.cash[0].to_f64() + self.borrowable())).max(0.0);
        for s in 1..self.slots.len() {
            if short <= TINY {
                break;
            }
            let spare = self.st.cash[s].to_f64() - self.unmet(k, s);
            if spare <= TINY {
                continue;
            }
            let (x, rate) = (self.st.fx[s], self.st.fx[s] * (1.0 - spread));
            let sold = spare.min(short / rate);
            self.st.cash[s] -= Cash::from_f64(sold);
            self.st.cash[0] += Cash::from_f64(sold * rate);
            acc.costs += sold * (x - rate);
            short -= sold * rate;
            self.log(t, EntryKind::Conversion, self.slots[s], s, -sold, sold * rate, Cause::Book, Some("covering a base obligation".into()));
        }
    }

    /// Draws on secured agreements that allow it, in order, up to what their collateral supports.
    /// One with a margin call outstanding lends nothing more.
    pub(super) fn draw(&mut self, k: usize, t: Ts) {
        let mut need = self.need(k);
        for j in 0..self.fin.len() {
            if need <= TINY {
                break;
            }
            let AgreementKind::Secured { draw: true, .. } = self.cfg.financing[j].kind else { continue };
            let st = &mut self.st.agreements[j];
            if st.call > 0.0 {
                continue;
            }
            let x = need.min((st.capacity - st.balance).max(0.0));
            if x > TINY {
                st.balance += x;
                self.st.cash[0] += Cash::from_f64(x);
                need -= x;
                self.log(t, EntryKind::Financing, EntityId::NONE, 0, 0.0, x, Cause::Agreement(j), Some("drawn".into()));
            }
        }
    }

    /// Sells base-currency holdings at their value less `cost_bp`: unpledged units first, in
    /// proportion; then each agreement's collateral in turn, whole, repaying the agreement from
    /// the proceeds before any reaches the book (a closeout).
    pub(super) fn sell(&mut self, k: usize, t: Ts, cost_bp: f64, acc: &mut Acc) {
        let c = cost_bp * 1e-4;
        let mut need = self.need(k);
        if need <= TINY {
            return;
        }
        let rows: Vec<(usize, f64)> = self
            .st
            .held
            .iter()
            .filter(|&&i| self.row_slot[i] == 0 && self.st.q[i] > 0.0)
            .map(|&i| (i, self.value(i, k)))
            .filter(|(_, v)| v.is_finite() && *v > 0.0)
            .collect();
        // Every unpledged holding, in any currency, at its value in base (a foreign one at its rate
        // less the conversion spread: its proceeds come home through `Convert`, which runs next).
        let spread = if self.cfg.conversion.auto { self.cfg.conversion.spread_bp * 1e-4 } else { 0.0 };
        let free: Vec<(usize, f64, f64, f64)> = self
            .st
            .held
            .iter()
            .filter(|&&i| self.st.q[i] > 0.0 && (self.row_slot[i] == 0 || self.cfg.conversion.auto))
            .map(|&i| (i, (self.st.q[i] - self.st.pledged[i].0).max(0.0), self.value(i, k)))
            .filter(|(_, u, v)| *u > TINY && v.is_finite() && *v > 0.0)
            .map(|(i, u, v)| {
                let s = self.row_slot[i];
                (i, u, v, if s == 0 { 1.0 } else { self.st.fx[s] * (1.0 - spread) })
            })
            .collect();
        let total: f64 = free.iter().map(|(_, u, v, x)| u * v * (1.0 - c) * x).sum();
        if total > TINY {
            let f = (need / total).min(1.0);
            for (i, units, v, _) in free {
                let (sold, s) = (units * f, self.row_slot[i]);
                self.st.q[i] -= sold;
                acc.costs += sold * v * c * self.st.fx[s];
                self.st.cash[s] += Cash::from_f64(sold * v * (1.0 - c));
                let e = self.records.entities()[i];
                self.log(t, EntryKind::Liquidation, e, s, -sold, sold * v * (1.0 - c), Cause::Book, Some("sold to meet a cash need".into()));
            }
            need = self.need(k);
        }
        for j in 0..self.fin.len() {
            if need <= TINY {
                break;
            }
            let mut proceeds = 0.0;
            for &(i, v) in &rows {
                let (units, to) = self.st.pledged[i];
                if to != j || units <= TINY {
                    continue;
                }
                let sold = units.min(self.st.q[i]);
                self.st.q[i] -= sold;
                self.st.pledged[i].0 = 0.0;
                acc.costs += sold * v * c;
                proceeds += sold * v * (1.0 - c);
                let e = self.records.entities()[i];
                self.log(t, EntryKind::Liquidation, e, 0, -sold, sold * v * (1.0 - c), Cause::Agreement(j), Some("collateral sold at closeout".into()));
            }
            let st = &mut self.st.agreements[j];
            let repaid = proceeds.min(st.balance);
            st.balance -= repaid;
            st.call = (st.call - repaid).max(0.0);
            self.st.cash[0] += Cash::from_f64(proceeds - repaid);
            if repaid > 0.0 {
                self.log(t, EntryKind::Financing, EntityId::NONE, 0, 0.0, -repaid, Cause::Agreement(j), Some("repaid from its collateral".into()));
            }
            need = self.need(k);
        }
        let held = self.st.held.clone();
        self.hold(held);
    }
}
