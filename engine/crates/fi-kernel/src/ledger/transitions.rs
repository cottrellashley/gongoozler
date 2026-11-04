//! Holding transitions (review H2), after the contract events and interest: at its time, every
//! unit of a contract becomes the units and cash its transition declares. The book's position
//! moves once, valued at the old contract's value and the new contracts' values at that time, and
//! the difference is booked to the transition's role. Units bought or sold and not yet delivered
//! become the same claim on what the contract turned into, and the cash they bring is owed on until
//! delivery, as a coupon is. Orders still working in the old contract are cancelled: it no longer
//! trades.

use super::*;
use crate::contract::Role;

impl Ledger<'_> {
    pub(super) fn transitions(&mut self, k: usize, t: Ts, acc: &mut Acc) -> Result<()> {
        let due: Vec<usize> = self.transition_k.iter().enumerate().filter(|(_, at)| **at == k).map(|(n, _)| n).collect();
        for n in due {
            self.transition(n, k, t, acc)?;
        }
        Ok(())
    }

    fn transition(&mut self, n: usize, k: usize, t: Ts, acc: &mut Acc) -> Result<()> {
        let rec = self.records;
        let tr = &rec.transitions()[n];
        let missing = |e: EntityId| Error::Missing(format!("{} cannot be valued at its transition at {t}", self.ws.entities.name(e)));
        let i = rec.idx(tr.from).ok_or_else(|| missing(tr.from))?;
        let undelivered: Vec<(usize, f64, usize)> = self.st.deliveries.iter().copied().filter(|(row, _, _)| *row == i).collect();
        let q = self.st.q[i];
        if q == 0.0 && undelivered.is_empty() {
            return Ok(());
        }
        let s = self.row_slot[i];
        let v_from = self.value(i, k);
        if !v_from.is_finite() {
            return Err(missing(tr.from));
        }
        let mut into = Vec::with_capacity(tr.into.len());
        for &(e, ratio) in &tr.into {
            let j = rec.idx(e).ok_or_else(|| missing(e))?;
            let v = self.value(j, k);
            if !v.is_finite() {
                return Err(missing(e));
            }
            into.push((j, ratio, v));
        }
        let fx = self.st.fx.clone();
        // The old contract leaves the book at its value now.
        let out = q * v_from * fx[s];
        let note = (!tr.note.is_empty()).then(|| tr.note.clone());
        self.log(t, EntryKind::Transition, tr.from, s, -q, -q * v_from, Cause::Transition(n), note);
        self.st.q[i] = 0.0;
        let mut came = 0.0;
        let mut rows = self.st.held.clone();
        for &(j, ratio, v) in &into {
            let units = q * ratio;
            self.st.q[j] += units;
            came += units * v * fx[self.row_slot[j]];
            rows.push(j);
            let e = rec.entities()[j];
            self.log(t, EntryKind::Transition, e, self.row_slot[j], units, units * v, Cause::Transition(n), None);
        }
        // Cash: what is delivered is the book's now; on units still to be delivered, the legal
        // holder receives it and owes it on until delivery (to the book on a purchase, by it on a
        // sale).
        let cash = tr.cash;
        if cash != 0.0 {
            let pending: f64 = undelivered.iter().filter(|(_, _, due)| *due > k).map(|(_, qty, _)| qty).sum();
            let settled = q - pending;
            self.st.cash[s] += Cash::product(settled, cash);
            self.log(t, EntryKind::Transition, EntityId::NONE, s, 0.0, settled * cash, Cause::Transition(n), None);
            for &(_, qty, due) in undelivered.iter().filter(|(_, _, due)| *due > k) {
                let amount = qty * cash;
                let note = if amount >= 0.0 { "owed by the seller until delivery" } else { "owed to the buyer until delivery" };
                let id = self.log(t, EntryKind::Transition, EntityId::NONE, s, 0.0, amount, Cause::Transition(n), Some(note.into()));
                self.st.dues.push(Due { id, k: due, slot: s, amount: Cash::product(qty, cash), income: Cash::ZERO });
            }
            came += q * cash * fx[s];
        }
        // Undelivered units of the old contract are the same claim on the new ones.
        let mut deliveries = Vec::with_capacity(self.st.deliveries.len());
        for &(row, qty, due) in &self.st.deliveries {
            if row == i {
                deliveries.extend(into.iter().map(|&(j, ratio, _)| (j, qty * ratio, due)));
            } else {
                deliveries.push((row, qty, due));
            }
        }
        self.st.deliveries = deliveries;
        // A legacy weight on the old contract moves to the new ones in proportion to their value.
        if let Some(pos) = self.st.weights.iter().position(|(row, _)| *row == i) {
            let (_, w) = self.st.weights.remove(pos);
            let total: f64 = into.iter().map(|&(j, ratio, v)| ratio * v * fx[self.row_slot[j]]).sum();
            if total > 0.0 {
                for &(j, ratio, v) in &into {
                    let share = w * ratio * v * fx[self.row_slot[j]] / total;
                    match self.st.weights.iter_mut().find(|(row, _)| *row == j) {
                        Some(x) => x.1 += share,
                        None => self.st.weights.push((j, share)),
                    }
                }
            }
        }
        // Orders still working in the old contract cannot fill.
        let mut cancelled = Vec::new();
        self.st.working.retain(|o| {
            let gone = o.row == i;
            if gone {
                cancelled.push(o.clone());
            }
            !gone
        });
        for o in cancelled {
            self.st.out.orders.cancelled += 1;
            self.log(t, EntryKind::Cancel, o.entity, s, o.remaining, 0.0, Cause::Transition(n), Some("its contract transitioned".into()));
            if self.cfg.journal {
                self.st.out.order_log.push(record(&o, Some(t), OrderStatus::Cancelled));
            }
        }
        let pnl = came - out;
        match tr.role {
            Role::Income => acc.income += pnl,
            Role::Performance => acc.price += pnl,
            Role::Principal => acc.principal += pnl,
            Role::Funding => acc.financing -= pnl,
            Role::Fee | Role::Protection | Role::Recovery | Role::Exchange => acc.other += pnl,
        }
        self.hold(rows);
        Ok(())
    }
}
