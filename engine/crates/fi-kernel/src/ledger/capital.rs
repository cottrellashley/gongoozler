//! Capital flows (subscriptions and redemptions, unitised) and legacy weight resets.

use super::*;

impl Ledger<'_> {
    pub(super) fn reset_weights(&mut self, k: usize) {
        let weights = std::mem::take(&mut self.st.weights);
        let nav = self.nav(k);
        let mut trade = vec![0.0; self.slots.len()];
        for &(i, w) in &weights {
            let v = self.value(i, k);
            let x = self.st.fx[self.row_slot[i]];
            if !v.is_finite() || v.abs() < f64::EPSILON {
                continue;
            }
            let target = w * nav / (v * x);
            trade[self.row_slot[i]] += (target - self.st.q[i]) * v;
            self.st.q[i] = target;
        }
        for (s, a) in trade.iter().enumerate() {
            self.st.cash[s] -= Cash::from_f64(*a);
        }
        let rows = self.st.held.iter().copied().chain(weights.iter().map(|(i, _)| *i)).collect();
        self.st.weights = weights;
        self.hold(rows);
    }

    pub(super) fn capital_flows(&mut self, k: usize, t: Ts) -> Result<f64> {
        let Some(flows) = self.cfg.flows.clone() else { return Ok(0.0) };
        let (f, settle) = match &flows {
            CapitalFlows::Series { settle, .. } => (self.flows.and_then(|(s, c)| s.exact(c, EntityId::NONE, t, t)).map_or(0.0, |h| h.value), *settle),
            CapitalFlows::Model { model, settle } => {
                let out = &self.st.out;
                let ctx = FlowCtx { t, k, nav: &out.nav, nav_per_unit: &out.nav_per_unit, view: View::new(self.ws, t, t) };
                (model.flow(&ctx)?, *settle)
            }
        };
        if f == 0.0 || !f.is_finite() {
            return Ok(0.0);
        }
        let id = self.log(t, EntryKind::Capital, EntityId::NONE, 0, 0.0, f, Cause::Capital, None);
        if settle == 0 {
            self.st.cash[0] += Cash::from_f64(f);
        } else {
            self.st.dues.push(Due { id, k: k + settle, slot: 0, amount: Cash::from_f64(f), income: Cash::ZERO });
        }
        Ok(f)
    }
}
