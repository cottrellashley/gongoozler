//! Settlement: receivables and payables due now move into cash, in the order they arose; margin calls
//! and overdue obligations are paid from what cash can spend.

use super::*;

impl Ledger<'_> {
    /// Settlement at time `t`. Receipts due arrive first; then payments due, or overdue, in the
    /// order they were booked. A payment is made from what its currency can spend now: settled
    /// cash, and any borrowing the book may use. A receivable not yet settled cannot fund it. What
    /// a payment cannot pay stays owed, overdue: cash never goes below its floor to pay it.
    pub(super) fn settle(&mut self, k: usize, t: Ts) {
        if self.st.dues.iter().any(|d| d.k <= k) {
            let dues = std::mem::take(&mut self.st.dues);
            let (now, later): (Vec<Due>, Vec<Due>) = dues.into_iter().partition(|d| d.k <= k);
            self.st.dues = later;
            let (receipts, mut payments): (Vec<Due>, Vec<Due>) = now.into_iter().partition(|d| d.amount >= Cash::ZERO);
            for d in receipts {
                if self.cfg.routing == Routing::Collections && d.income != Cash::ZERO {
                    self.st.interest_collections[d.slot] += d.income;
                    self.st.cash[d.slot] += d.amount - d.income;
                } else {
                    self.st.cash[d.slot] += d.amount;
                }
                if self.cfg.journal {
                    let e = self.slots[d.slot];
                    self.log(t, EntryKind::Settle, e, d.slot, 0.0, d.amount.to_f64(), Cause::Due(d.id), None);
                }
            }
            payments.sort_by_key(|d| (d.k, d.id));
            for d in payments {
                self.pay_due(t, d);
            }
        }
        self.st.deliveries.retain(|&(_, _, due)| due > k);
    }

    /// What slot `slot` can spend now: its settled cash, and in the base currency any borrowing
    /// the book may use. `None`: without limit.
    pub(super) fn spendable_now(&self, slot: usize) -> Option<Cash> {
        let floor = if slot == 0 { self.borrowable() } else { 0.0 };
        floor.is_finite().then(|| (self.st.cash[slot] + Cash::from_f64(floor)).max(Cash::ZERO))
    }

    /// Pays a payable as far as its currency's cash allows; the rest stays owed, overdue.
    pub(super) fn pay_due(&mut self, t: Ts, d: Due) {
        let owed = -d.amount;
        let pay = self.spendable_now(d.slot).map_or(owed, |can| owed.min(can));
        if pay > Cash::ZERO {
            self.st.cash[d.slot] -= pay;
            if self.cfg.journal {
                let e = self.slots[d.slot];
                self.log(t, EntryKind::Settle, e, d.slot, 0.0, -pay.to_f64(), Cause::Due(d.id), (pay < owed).then(|| "paid in part".into()));
            }
        }
        if pay < owed {
            self.st.dues.push(Due { amount: d.amount + pay, ..d });
        }
    }

    /// Pays agreement `j`'s due call as far as base cash allows; the rest stays called, overdue.
    pub(super) fn pay_call(&mut self, j: usize, t: Ts) {
        let call = self.st.agreements[j].call;
        let pay = self.spendable_now(0).map_or(call, |can| call.min(can.to_f64()));
        if pay <= 0.0 {
            return;
        }
        let st = &mut self.st.agreements[j];
        st.balance -= pay;
        st.call = if pay >= call { 0.0 } else { call - pay };
        st.paid += pay;
        self.st.cash[0] -= Cash::from_f64(pay);
        let why = match (st.matured, pay < call) {
            (true, true) => "repaid in part at maturity",
            (true, false) => "repaid at maturity",
            (false, true) => "margin call paid in part",
            (false, false) => "margin call paid",
        };
        self.log(t, EntryKind::Financing, EntityId::NONE, 0, 0.0, -pay, Cause::Agreement(j), Some(why.into()));
    }

    /// Pays what is due and still owed (payables, then margin calls), from cash raised since.
    pub(super) fn pay_overdue(&mut self, k: usize, t: Ts) {
        if self.st.dues.iter().any(|d| d.k <= k && d.amount < Cash::ZERO) {
            let dues = std::mem::take(&mut self.st.dues);
            let (mut now, later): (Vec<Due>, Vec<Due>) = dues.into_iter().partition(|d| d.k <= k && d.amount < Cash::ZERO);
            self.st.dues = later;
            now.sort_by_key(|d| (d.k, d.id));
            for d in now {
                self.pay_due(t, d);
            }
        }
        for j in 0..self.st.agreements.len() {
            let a = &self.st.agreements[j];
            if a.call > 0.0 && k >= a.call_due {
                self.pay_call(j, t);
            }
        }
    }

    /// Obligations due by step `k` and still unpaid in slot `slot`: payables, and in the base
    /// currency margin calls.
    pub(super) fn unmet(&self, k: usize, slot: usize) -> f64 {
        let mut u: f64 = self.st.dues.iter().filter(|d| d.slot == slot && d.k <= k && d.amount < Cash::ZERO).map(|d| -d.amount.to_f64()).sum();
        if slot == 0 {
            u += self.st.agreements.iter().filter(|a| a.call > 0.0 && k >= a.call_due).map(|a| a.call).sum::<f64>();
        }
        u
    }
}
