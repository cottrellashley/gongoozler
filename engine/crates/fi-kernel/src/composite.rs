//! Composites: a portfolio held as one contract (axiom 4).
//!
//! Continuous futures, rolled CDX, constant-maturity buckets, hedged sleeves and benchmarks are
//! all composites. A roll is a policy:
//!
//! * held **whole**, the roll happens inside the unit, with a modelled cost;
//! * held **expanded**, the same roll is a set of trades in the legs.
//!
//! With equal costs both give the same P&L (invariant I3).

use crate::{Error, Result, contract::Terms, entity::EntityId, records::UnitRecords, time::Ts};

/// A composite: legs, value weights from given times, and a roll cost.
#[derive(Clone, Debug)]
pub struct Composite {
    /// The composite's own entity.
    pub entity: EntityId,
    /// Its legs (contracts with records).
    pub legs: Vec<EntityId>,
    /// From each time, the legs' value weights (each vector as long as `legs`). The first entry
    /// must be at or before the calendar's first time; later entries are rolls.
    pub schedule: Vec<(Ts, Vec<f64>)>,
    /// Cost of rolling, in basis points of the value traded.
    pub roll_cost_bp: f64,
}

impl Composite {
    /// One composite unit's records (worth 1 at the first time), and the legs' quantities per
    /// composite unit after each time (what an expanded holder trades towards).
    ///
    /// # Errors
    /// Unknown legs, weights of the wrong length, or no weights at the first time.
    pub fn build(&self, legs: &UnitRecords) -> Result<(UnitRecords, Vec<Vec<f64>>)> {
        let cal = legs.calendar().clone();
        let times = cal.times();
        if times.is_empty() {
            return Err(Error::Invalid("composite over an empty calendar".into()));
        }
        let rows: Vec<usize> =
            self.legs.iter().map(|e| legs.idx(*e).ok_or_else(|| Error::Unknown(format!("composite leg {} has no records", e.0)))).collect::<Result<_>>()?;
        let mut schedule = self.schedule.clone();
        schedule.sort_by_key(|(t, _)| *t);
        if schedule.iter().any(|(_, w)| w.len() != rows.len()) {
            return Err(Error::Invalid("every weight vector needs one weight per leg".into()));
        }
        let first =
            schedule.iter().rposition(|(t, _)| *t <= times[0]).ok_or_else(|| Error::Invalid("the composite has no weights at its first time".into()))?;
        let v = |j: usize, k: usize| legs.value[legs.at(rows[j], k)];
        let nl = rows.len();
        let mut q: Vec<f64> = (0..nl).map(|j| schedule[first].1[j] / v(j, 0)).collect();
        let tn = times.len();
        let (mut value, mut cash, mut terms, mut known, mut state) = (vec![0.0; tn], vec![0.0; tn], vec![Terms::default(); tn], vec![0i64; tn], vec![0u8; tn]);
        let mut qs = Vec::with_capacity(tn);
        value[0] = (0..nl).map(|j| q[j] * v(j, 0)).sum();
        known[0] = (0..nl).map(|j| legs.known[legs.at(rows[j], 0)]).max().unwrap_or(i64::MIN);
        qs.push(q.clone());
        for k in 1..tn {
            let mut tm = Terms::default();
            for j in 0..nl {
                let lt = legs.terms(rows[j], k);
                tm.income += q[j] * lt.income;
                tm.price += q[j] * lt.price;
                tm.principal += q[j] * lt.principal;
                tm.other += q[j] * lt.other;
                tm.financing += q[j] * lt.financing;
                cash[k] += q[j] * legs.cash[legs.at(rows[j], k)];
                known[k] = known[k].max(legs.known[legs.at(rows[j], k)]);
                state[k] = state[k].max(legs.state[legs.at(rows[j], k)]);
            }
            let v_pre: f64 = (0..nl).map(|j| q[j] * v(j, k)).sum();
            value[k] = v_pre;
            if let Some((_, w)) = schedule.iter().find(|(t, _)| *t == times[k]) {
                // Value after cost: v = v_pre − bp × Σ |w_j v − q_j V_j|, solved by fixed point.
                let bp = self.roll_cost_bp * 1e-4;
                let mut after = v_pre;
                for _ in 0..64 {
                    let cost: f64 = (0..nl).map(|j| (w[j] * after - q[j] * v(j, k)).abs()).sum::<f64>() * bp;
                    let next = v_pre - cost;
                    let done = (next - after).abs() <= 1e-15 * v_pre.abs().max(1.0);
                    after = next;
                    if done {
                        break;
                    }
                }
                tm.other -= v_pre - after;
                q = (0..nl).map(|j| w[j] * after / v(j, k)).collect();
                value[k] = after;
            }
            terms[k] = tm;
            qs.push(q.clone());
        }
        let records = UnitRecords::from_parts(cal, vec![self.entity], value, cash, terms, known, state)?;
        Ok((records, qs))
    }
}
