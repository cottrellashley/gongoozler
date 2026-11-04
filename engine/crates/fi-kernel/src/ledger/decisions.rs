//! Decisions and orders: each scheduled policy decides, by priority, from what it may see; intentions
//! due now become working orders, a newer decision cancelling its policy's older orders.

use super::*;

impl Ledger<'_> {
    pub(super) fn decide(&mut self, k: usize, t: Ts, nav: f64) -> Result<()> {
        let order = self.order.clone();
        for p in order {
            if self.policies[p].schedule.binary_search(&t).is_err() {
                continue;
            }
            let holdings: Vec<(usize, f64)> = self.st.held.iter().map(|&i| (i, self.st.q[i])).collect();
            let by = k + self.policies[p].lag;
            let cash: Vec<(EntityId, f64)> = (0..self.slots.len()).map(|s| (self.slots[s], self.spendable(s, by))).collect();
            let ctx = DecisionCtx {
                t,
                k,
                view: View::new(self.ws, t, t),
                records: self.records,
                holdings: &holdings,
                nav,
                orders: &self.st.working,
                cash: &cash,
                policy: p,
                notes: &self.notes,
                state: &self.st.published,
            };
            let targets = self.policies[p].decide.decide(&ctx)?;
            let notes: Vec<String> = self.notes.lock().map(|mut n| std::mem::take(&mut *n)).unwrap_or_default();
            for n in notes {
                self.log(t, EntryKind::Note, EntityId::NONE, 0, 0.0, 0.0, Cause::Policy(p), Some(n));
            }
            if let Some(targets) = targets {
                self.st.queued.push_back(Queued { due: by, policy: p, targets });
            }
        }
        Ok(())
    }

    pub(super) fn row(&self, e: EntityId) -> Result<usize> {
        self.records.idx(e).ok_or_else(|| Error::Unknown(format!("decision names {}, which has no contract", self.ws.entities.name(e))))
    }

    /// Desired quantities for a policy's targets, its new weights, and whether the targets are
    /// explicit orders.
    #[allow(clippy::type_complexity)]
    pub(super) fn desired(&mut self, k: usize, t: Ts, nav: f64, targets: &Targets) -> Result<(Vec<(usize, f64)>, Vec<(usize, f64)>, bool)> {
        let mut desired = Vec::new();
        let mut weights = Vec::new();
        let mut explicit = false;
        match targets {
            Targets::Weights(w) => {
                for (e, wi) in w {
                    let i = self.row(*e)?;
                    let v = self.value(i, k) * self.st.fx[self.row_slot[i]];
                    if !v.is_finite() || v.abs() < f64::EPSILON {
                        self.st.out.skipped_trades += 1;
                        desired.push((i, self.st.q[i]));
                        continue;
                    }
                    desired.push((i, wi * nav / v));
                    weights.push((i, *wi));
                }
            }
            Targets::Quantities(q) => {
                for (e, qi) in q {
                    desired.push((self.row(*e)?, *qi));
                }
            }
            Targets::InMeasure { measure, targets } => {
                let view = View::new(self.ws, t, t);
                for (e, target) in targets {
                    let i = self.row(*e)?;
                    match measure.per_unit(self.records, i, k, &view) {
                        Some(m) if m.is_finite() && m.abs() > f64::EPSILON => desired.push((i, target / m)),
                        _ => {
                            self.st.out.skipped_trades += 1;
                            desired.push((i, self.st.q[i]));
                        }
                    }
                }
            }
            Targets::Orders(o) => {
                explicit = true;
                for (e, dq) in o {
                    let i = self.row(*e)?;
                    desired.push((i, self.st.q[i] + dq));
                }
            }
        }
        desired.sort_unstable_by_key(|(i, _)| *i);
        if let Some(w) = desired.windows(2).find(|w| w[0].0 == w[1].0) {
            return Err(Error::Invalid(format!("decision at {t} names {} twice", self.ws.entities.name(self.records.entities()[w[0].0]))));
        }
        weights.sort_unstable_by_key(|(i, _)| *i);
        Ok((desired, weights, explicit))
    }

    pub(super) fn submit(&mut self, k: usize, t: Ts, nav: f64) -> Result<()> {
        if !self.st.queued.front().is_some_and(|q| q.due <= k) {
            return Ok(());
        }
        let mut due: Vec<Queued> = Vec::new();
        while self.st.queued.front().is_some_and(|q| q.due <= k) {
            due.push(self.st.queued.pop_front().expect("checked"));
        }
        due.sort_by_key(|q| (std::cmp::Reverse(self.policies[q.policy].priority), q.policy));
        let mut claimed: Vec<bool> = vec![false; self.st.q.len()];
        let latency = self.exec.latency();
        for item in due {
            let p = item.policy;
            let (mut desired, weights, explicit) = self.desired(k, t, nav, &item.targets)?;
            // Held contracts in the policy's scope that it no longer targets are sold.
            if !explicit {
                let targeted: Vec<usize> = desired.iter().map(|(i, _)| *i).collect();
                let scope = self.policies[p].scope.as_ref();
                for &i in &self.st.held {
                    if scope.is_none_or(|m| m[i]) && targeted.binary_search(&i).is_err() {
                        desired.push((i, 0.0));
                    }
                }
                desired.sort_unstable_by_key(|(i, _)| *i);
            }
            if let Some(m) = &self.policies[p].scope {
                desired.retain(|(i, _)| m[*i]);
            }
            // A higher-priority policy trading a contract this step keeps it.
            let mut kept = Vec::with_capacity(desired.len());
            for (i, d) in desired {
                if claimed[i] {
                    let e = self.records.entities()[i];
                    let note = format!("{} left to a higher-priority policy", self.ws.entities.name(e));
                    self.log(t, EntryKind::Note, e, 0, 0.0, 0.0, Cause::Policy(p), Some(note));
                } else {
                    kept.push((i, d));
                }
            }
            // A newer decision replaces the policy's older orders on these contracts.
            let rows: Vec<usize> = kept.iter().map(|(i, _)| *i).collect();
            let mut cancelled = Vec::new();
            self.st.working.retain(|o| {
                let replace = o.policy == p && rows.binary_search(&o.row).is_ok();
                if replace {
                    cancelled.push(o.clone());
                }
                !replace
            });
            for o in cancelled {
                self.st.out.orders.cancelled += 1;
                self.log(t, EntryKind::Cancel, o.entity, self.row_slot[o.row], o.remaining, 0.0, Cause::Order(o.id), None);
                if self.cfg.journal {
                    self.st.out.order_log.push(record(&o, Some(t), OrderStatus::Cancelled));
                }
            }
            let basket = {
                let mut h = Hash64::new();
                h.u64(p as u64);
                h.u64(k as u64);
                h.finish()
            };
            let tif = self.policies[p].tif;
            let q = &self.st.q;
            kept.sort_by_key(|(i, d)| (*d - q[*i] > 0.0, *i));
            for (i, d) in kept {
                claimed[i] = true;
                let delta = d - self.st.q[i];
                if delta.abs() <= TINY * d.abs().max(1.0) {
                    continue;
                }
                let e = self.records.entities()[i];
                let s = self.row_slot[i];
                let v = self.value(i, k);
                let id = self.log(t, EntryKind::Order, e, s, delta, 0.0, Cause::Policy(p), None);
                self.st.out.orders.submitted += 1;
                if v.is_finite() {
                    self.st.out.orders.requested_value += (delta * v * self.st.fx[s]).abs();
                }
                self.st.working.push(WorkingOrder {
                    id,
                    entity: e,
                    row: i,
                    remaining: delta,
                    requested: delta,
                    filled: 0.0,
                    paid: 0.0,
                    submitted: t,
                    submitted_k: k,
                    workable_k: k + latency,
                    expires_k: k + latency + tif - 1,
                    attempts: 0,
                    policy: p,
                    basket,
                    decision_value: v,
                });
            }
            if p == 0 && self.cfg.holding == HoldingPolicy::ResetWeights && matches!(item.targets, Targets::Weights(_)) {
                self.st.weights = weights;
            }
        }
        Ok(())
    }
}
