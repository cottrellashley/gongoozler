//! Phase 1 of a step, before anything else: what the held units paid and earned over the interval (their
//! flows by role, translated at the new FX rates; a coupon gone ex booked as a receivable; flows on
//! units traded but not delivered owed on to their owner), then interest on cash and financing, and
//! borrow fees.

use super::*;

impl Ledger<'_> {
    pub(super) fn contract_events(&mut self, k: usize, t: Ts, fx_now: &[f64], acc: &mut Acc) {
        let rec = self.records;
        let collections = self.cfg.routing == Routing::Collections;
        // Translation of the cash, collections and dues held over the interval in other
        // currencies. This step's flows arrive at its end, already at the new rate: they are
        // not translated.
        for (s, x1) in fx_now.iter().enumerate().skip(1) {
            let dx = x1 - self.st.fx[s];
            if dx == 0.0 {
                continue;
            }
            let mut bal = self.st.cash[s].to_f64() + self.st.interest_collections[s].to_f64();
            for d in &self.st.dues {
                if d.slot == s {
                    bal += d.amount.to_f64();
                }
            }
            acc.currency += bal * dx;
        }
        let held = std::mem::take(&mut self.st.held);
        let mut owed: Vec<(usize, usize, f64, f64, EntityId)> = Vec::new();
        for &i in &held {
            let q = self.st.q[i];
            let at = rec.at(i, k);
            let s = self.row_slot[i];
            let (x0, x1) = (self.st.fx[s], fx_now[s]);
            acc.income += q * rec.income[at] * x1;
            acc.price += q * rec.price[at] * x1;
            acc.principal += q * rec.principal[at] * x1;
            acc.other += q * rec.other[at] * x1;
            acc.financing += q * rec.financing[at] * x1;
            if x1 != x0 {
                acc.currency += q * rec.value[at - 1] * (x1 - x0);
            }
            let drop = rec.face[at - 1] - rec.face[at];
            if drop.is_finite() {
                self.st.collected.par_drop += q * drop * x1;
            }
            let deferred = rec.deferred_cash[at];
            if deferred != 0.0 {
                let pay = Ts(rec.deferred_at[at]);
                let due = rec.calendar().times().partition_point(|x| *x < pay);
                let e = rec.entities()[i];
                let amount = q * deferred;
                let id = self.log(t, EntryKind::Flow, e, s, q, amount, Cause::Contract(e), Some(format!("entitled; pays {pay}")));
                self.st.dues.push(Due { id, k: due, slot: s, amount: Cash::product(q, deferred), income: Cash::product(q, rec.deferred_income[at]) });
            }
            let unit = rec.cash[at];
            if unit == 0.0 {
                continue;
            }
            self.st.collected.interest += q * rec.cash_income[at] * x1;
            self.st.collected.principal += q * (unit - rec.cash_income[at]) * x1;
            // Units traded but not yet settled: the legal holder receives the flow and owes it to
            // the economic owner until settlement (the coupon goes to the economic owner once).
            // A purchase's seller owes the book; on a sale, the book owes its buyer.
            let mut settled_q = q;
            for &(row, qty, due) in &self.st.deliveries {
                if row == i && due > k {
                    settled_q -= qty;
                    owed.push((due, s, qty, unit, rec.entities()[i]));
                }
            }
            if settled_q != 0.0 {
                // Quantity times what a unit pays, in decimal (review H4).
                let paid = Cash::product(settled_q, unit);
                if collections {
                    let inc = Cash::product(settled_q, rec.cash_income[at]);
                    self.st.interest_collections[s] += inc;
                    self.st.cash[s] += paid - inc;
                } else {
                    self.st.cash[s] += paid;
                }
            }
            if self.cfg.journal {
                let e = rec.entities()[i];
                self.log(t, EntryKind::Flow, e, s, q, q * unit, Cause::Contract(e), None);
            }
        }
        self.st.held = held;
        for (due, s, qty, unit, e) in owed {
            let amount = qty * unit;
            let note = if amount >= 0.0 { "owed by the seller until settlement" } else { "owed to the buyer until settlement" };
            let id = self.log(t, EntryKind::Flow, e, s, 0.0, amount, Cause::Contract(e), Some(note.into()));
            self.st.dues.push(Due { id, k: due, slot: s, amount: Cash::product(qty, unit), income: Cash::ZERO });
        }
    }

    pub(super) fn interest(&mut self, k: usize, t: Ts, acc: &mut Acc, fx_now: &[f64]) {
        let t0 = self.records.calendar().times()[k - 1];
        let dt = (t.0 - t0.0) as f64 / Ts::DAY as f64 / 360.0;
        let c = self.st.cash[0].to_f64();
        let rate = |col: Col<'_>| read(col, EntityId::NONE, t0, Ts::MAX).filter(|r| r.is_finite());
        if c > 0.0
            && let Some(r) = rate(self.deposit)
        {
            let i = c * r * dt;
            self.st.cash[0] += Cash::from_f64(i);
            acc.income += i * fx_now[0];
            self.log(t, EntryKind::Interest, EntityId::NONE, 0, 0.0, i, Cause::Interest, Some("deposit".into()));
        }
        if c < 0.0
            && let Some(r) = rate(self.borrow_rate)
        {
            let i = c * r * dt;
            self.st.cash[0] += Cash::from_f64(i);
            acc.financing -= i * fx_now[0];
            self.log(t, EntryKind::Interest, EntityId::NONE, 0, 0.0, i, Cause::Interest, Some("borrowing".into()));
        }
        for j in 0..self.fin.len() {
            let b = self.st.agreements[j].balance;
            if b > 0.0
                && let Some(r) = rate(self.fin[j].rate)
            {
                let i = b * r * dt;
                self.st.cash[0] -= Cash::from_f64(i);
                acc.financing += i * fx_now[0];
                let name = self.cfg.financing[j].name.clone();
                self.log(t, EntryKind::Interest, EntityId::NONE, 0, 0.0, -i, Cause::Agreement(j), Some(name));
            }
        }
        if let ShortPolicy::Borrow { fee_bp, .. } = self.cfg.shorts
            && fee_bp != 0.0
        {
            let held = self.st.held.clone();
            for i in held {
                let q = self.st.q[i];
                if q < 0.0 && self.records.needs_borrow[i] {
                    let s = self.row_slot[i];
                    let fee = -q * self.value(i, k - 1) * fee_bp * 1e-4 * dt;
                    self.st.cash[s] -= Cash::from_f64(fee);
                    acc.financing += fee * fx_now[s];
                    let e = self.records.entities()[i];
                    self.log(t, EntryKind::Interest, e, s, q, -fee, Cause::Interest, Some("borrow fee".into()));
                }
            }
        }
    }
}
