//! Execution: the orders workable now go to the execution model together, and its fills book trades
//! that settle on their dates, within the cash and short guards.

use super::*;

impl Ledger<'_> {
    /// Moves out the orders that can be worked now (sales first, so their proceeds can fund
    /// purchases), with each unit's value.
    pub(super) fn take_workable(&mut self, k: usize) -> (Vec<WorkingOrder>, Vec<f64>) {
        if self.st.working.is_empty() {
            return (Vec::new(), Vec::new());
        }
        let all = std::mem::take(&mut self.st.working);
        let (mut batch, rest): (Vec<WorkingOrder>, Vec<WorkingOrder>) = all.into_iter().partition(|o| o.workable_k <= k);
        self.st.working = rest;
        let key = |o: &WorkingOrder| (o.remaining > 0.0, o.row, o.id);
        if !batch.is_sorted_by_key(key) {
            batch.sort_unstable_by_key(key);
        }
        let values = batch.iter().map(|o| self.value(o.row, k)).collect();
        (batch, values)
    }

    /// The largest purchase cost the cash guard allows for `o` (converting base currency first
    /// where the trade is in another currency).
    /// The step a trade settles at: the first time at or after the instant its quote stipulates
    /// (beyond the calendar: never, within the run), else the book's lag.
    pub(super) fn settles(&self, k: usize, i: usize, fill: &Fill) -> usize {
        match fill.settle_at {
            Some(at) => k.max(self.records.calendar().times().partition_point(|x| *x < at)),
            None => k + self.settle_lag[i],
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn affordable(&mut self, k: usize, t: Ts, i: usize, id: u64, cost: f64, by: usize, acc: &mut Acc) -> f64 {
        let s = self.row_slot[i];
        let mut have = self.spendable(s, by);
        if cost > have + TINY && s != 0 && self.cfg.conversion.auto {
            let x = self.st.fx[s];
            let need = cost - have.max(0.0);
            let rate = x * (1.0 + self.cfg.conversion.spread_bp * 1e-4);
            let base = self.spendable(0, by).max(0.0) + self.borrowable();
            let got = need.min(base / rate);
            if got > 0.0 {
                // An FX trade that settles with the purchase: base cash leaves only when the cash
                // it counted on (sale proceeds settling by then) has arrived.
                let cid = self.log(t, EntryKind::Conversion, self.slots[s], s, got, got * rate, Cause::Order(id), None);
                if by == k {
                    self.st.cash[0] -= Cash::from_f64(got * rate);
                    self.st.cash[s] += Cash::from_f64(got);
                } else {
                    self.st.dues.push(Due { id: cid, k: by, slot: 0, amount: Cash::from_f64(-got * rate), income: Cash::ZERO });
                    self.st.dues.push(Due { id: cid, k: by, slot: s, amount: Cash::from_f64(got), income: Cash::ZERO });
                }
                acc.costs += got * (rate - x);
                have = self.spendable(s, by);
            }
        }
        if s == 0 { have + self.borrowable() } else { have }
    }

    /// Books the fills of a batch, within the short and cash guards.
    pub(super) fn apply(&mut self, k: usize, t: Ts, batch: Vec<WorkingOrder>, values: &[f64], fills: &[Fill], acc: &mut Acc) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        let nav = self.nav(k);
        let mut touched = self.st.held.clone();
        for ((mut order, &v), fill) in batch.into_iter().zip(values).zip(fills) {
            let (i, id, entity, decision_value, remaining) = (order.row, order.id, order.entity, order.decision_value, order.remaining);
            let s = self.row_slot[i];
            let x = self.st.fx[s];
            let mut f = if fill.qty.signum() == remaining.signum() { fill.qty.abs().min(remaining.abs()).copysign(remaining) } else { 0.0 };
            if !v.is_finite() {
                self.st.out.skipped_trades += 1;
                f = 0.0;
            }
            // The short guard: securities are sold short only against a borrow.
            if f < 0.0 && self.records.needs_borrow[i] && self.st.q[i] + f < -TINY {
                let held = self.st.q[i].max(0.0);
                let allowed = match &self.cfg.shorts {
                    ShortPolicy::Forbid => held,
                    ShortPolicy::Borrow { .. } => held + read(self.available, entity, t, t).map_or(f64::INFINITY, |a| (a + self.st.q[i].min(0.0)).max(0.0)),
                    ShortPolicy::Unlimited => f64::INFINITY,
                };
                if -f > allowed {
                    f = -allowed;
                    self.st.out.short_limited += 1;
                    self.log(t, EntryKind::Breach, entity, s, f, 0.0, Cause::Order(id), Some("short guard: no borrow".into()));
                }
            }
            let due = self.settles(k, i, fill);
            // The cash guard: a purchase is paid from cash it can spend by its settlement.
            if f > 0.0 && !matches!(self.cfg.cash, CashPolicy::Unlimited) {
                let cost = f * fill.price;
                let room = self.affordable(k, t, i, id, cost, due, acc);
                if cost > room + TINY * cost.max(1.0) {
                    match breach_of(&self.cfg.cash) {
                        Breach::Fail => {
                            return Err(Error::Invalid(format!(
                                "buying {} at {t} needs {cost:.2} but only {room:.2} can be spent",
                                self.ws.entities.name(entity)
                            )));
                        }
                        b => {
                            let scaled = if b == Breach::Refuse || fill.price <= 0.0 { 0.0 } else { (room / fill.price).max(0.0) };
                            if scaled < f {
                                f = scaled;
                                self.st.out.cash_limited += 1;
                                self.log(t, EntryKind::Breach, entity, s, f, room, Cause::Order(id), Some("cash guard".into()));
                            }
                        }
                    }
                }
            }
            order.attempts += 1;
            if f != 0.0 {
                self.st.q[i] += f;
                touched.push(i);
                acc.costs += f * (fill.price - v) * x;
                acc.turnover += (f * v * x).abs() / nav.abs().max(f64::EPSILON);
                if decision_value.is_finite() {
                    self.st.out.orders.delay_cost += f * (v - decision_value) * x;
                    self.st.out.orders.filled_value += (f * decision_value * x).abs();
                }
                let amount = -f * fill.price;
                let fid = self.log(t, EntryKind::Fill, entity, s, f, amount, Cause::Order(id), None);
                // Units times the price, in decimal (review H4).
                let cash = Cash::product(-f, fill.price);
                if due == k {
                    self.st.cash[s] += cash;
                } else {
                    // Cash and units change hands at settlement: until then the sale's proceeds
                    // are a receivable (a purchase's cost a payable), and the units are still the
                    // seller's to deliver.
                    self.st.dues.push(Due { id: fid, k: due, slot: s, amount: cash, income: Cash::ZERO });
                    self.st.deliveries.push((i, f, due));
                }
                self.st.out.trades += 1;
                order.remaining -= f;
                order.filled += f;
                order.paid += f * fill.price;
            }
            let status = if order.remaining.abs() <= TINY * order.requested.abs().max(1.0) {
                Some(OrderStatus::Filled)
            } else if k >= order.expires_k {
                Some(if order.filled == 0.0 { OrderStatus::Expired } else { OrderStatus::Partial })
            } else {
                None
            };
            match status {
                Some(st) => {
                    match st {
                        OrderStatus::Filled => self.st.out.orders.filled += 1,
                        OrderStatus::Partial => self.st.out.orders.partial += 1,
                        _ => self.st.out.orders.expired += 1,
                    }
                    if st != OrderStatus::Filled {
                        self.st.out.unfilled_orders += 1;
                        if decision_value.is_finite() && v.is_finite() {
                            self.st.out.orders.missed += order.remaining * (v - decision_value) * x;
                        }
                        self.log(t, EntryKind::Expire, entity, s, order.remaining, 0.0, Cause::Order(id), None);
                    }
                    if self.cfg.journal {
                        self.st.out.order_log.push(record(&order, Some(t), st));
                    }
                }
                None => self.st.working.push(order),
            }
        }
        self.hold(touched);
        Ok(())
    }
}
