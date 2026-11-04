//! Distribution: the distributor's payments out of the interest and principal accounts (a deal's
//! waterfall, a fund's distributions).

use super::*;

impl Ledger<'_> {
    /// The distributor's payments out of the base currency's interest and principal accounts:
    /// what left the book, net of contributions.
    pub(super) fn distribute(&mut self, k: usize, t: Ts) -> Result<f64> {
        let Some(d) = self.cfg.distributor.clone() else { return Ok(0.0) };
        let mut state = self.st.dist.take().ok_or_else(|| Error::Invalid("the distributor's state is missing".into()))?;
        let mut dist = Distribution {
            t,
            k,
            records: self.records,
            view: View::new(self.ws, t, t),
            holdings: self.st.held.iter().map(|&i| (i, self.st.q[i])).collect(),
            collected: self.st.collected,
            nav: self.nav(k),
            interest: self.st.interest_collections[0].to_f64(),
            principal: self.st.cash[0].to_f64(),
            paid: 0.0,
            liquidated: false,
            fx: self.st.fx.clone(),
            row_slot: self.row_slot.clone(),
            notes: Vec::new(),
            published: self.st.published.clone(),
        };
        let r = d.0.distribute(&mut dist, state.as_mut());
        self.st.dist = Some(state);
        r?;
        if dist.liquidated {
            let held = std::mem::take(&mut self.st.held);
            for i in held {
                let q = std::mem::take(&mut self.st.q[i]);
                let e = self.records.entities()[i];
                let v = self.value(i, k);
                self.log(t, EntryKind::Liquidation, e, self.row_slot[i], -q, q * v, Cause::Book, Some("sold at the mark under the book's terms".into()));
            }
            // Holdings in other currencies were sold into base cash at the current rates.
        }
        self.st.interest_collections[0] = Cash::from_f64(dist.interest);
        self.st.cash[0] = Cash::from_f64(dist.principal);
        self.st.published = std::mem::take(&mut dist.published);
        for n in std::mem::take(&mut dist.notes) {
            self.log(t, EntryKind::Note, EntityId::NONE, 0, 0.0, 0.0, Cause::Book, Some(n));
        }
        if dist.paid != 0.0 {
            self.log(t, EntryKind::Distribution, EntityId::NONE, 0, 0.0, -dist.paid, Cause::Book, None);
        }
        Ok(dist.paid)
    }
}
