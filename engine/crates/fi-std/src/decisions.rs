//! Example decision models: what a strategy decides at each rebalance.
//!
//! * [`TopN`]: equal weights in the top N by a signal.
//! * [`RiskParity`]: weights across sleeves from their own returns (inverse volatility or equal
//!   risk contribution), an asset-allocation model.
//! * [`Replay`]: recorded weights or quantities from a series, so a legacy backtester's book can be
//!   booked by the ledger unchanged (depth 0 of bringing it in).
//! * [`Trades`]: recorded trades as orders (a holdings replay).
//! * [`Branch`]: one decision model until a time, another from it (an intervention).
//! * [`Reinvest`]: a deal manager spending principal cash while the deal reinvests.
//! * [`ExitAbove`]: a forced seller's mandate, such as an index floor's fallen-angel rule.

use std::sync::Arc;

use fi_kernel::{DecisionCtx, DecisionModel, EntityId, Error, Producer, Result, Targets, Ts, Workspace, hash::Hash64};

/// Whether `series` is an observation of what happened: registered data, with no scenario upstream.
fn observed(ws: &Workspace, series: &str) -> bool {
    ws.series(series).is_ok_and(|s| s.provenance().producer == Producer::Observed && s.provenance().intervention.is_none())
}

/// Equal weights in the top `n` entities by a signal known at the decision.
#[derive(Clone, Debug)]
pub struct TopN {
    /// Signal series.
    pub series: String,
    /// Signal column.
    pub column: String,
    /// Candidates (contracts).
    pub universe: Vec<EntityId>,
    /// How many to hold.
    pub n: usize,
    /// Total weight.
    pub gross: f64,
}

impl DecisionModel for TopN {
    fn name(&self) -> String {
        format!("top{}({}.{})", self.n, self.series, self.column)
    }

    fn params(&self) -> u64 {
        let mut h = Hash64::new();
        h.str(&self.series);
        h.str(&self.column);
        h.u64(self.n as u64);
        h.f64(self.gross);
        h.finish()
    }

    fn decide(&self, ctx: &DecisionCtx<'_>) -> Result<Option<Targets>> {
        let s = ctx.view.series(&self.series)?;
        let c = s.column(&self.column)?;
        let values = ctx.view.cross_section(s, c, &self.universe);
        let mut scored: Vec<(f64, EntityId)> = values.into_iter().zip(&self.universe).filter(|(v, _)| v.is_finite()).map(|(v, e)| (v, *e)).collect();
        if scored.is_empty() {
            return Ok(None);
        }
        // Best first, ties by entity: a total order, so the selection is deterministic. Selecting
        // the top n first costs O(universe) rather than a full sort.
        let by = |a: &(f64, EntityId), b: &(f64, EntityId)| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1));
        let n = self.n.min(scored.len());
        if n > 0 && n < scored.len() {
            scored.select_nth_unstable_by(n - 1, by);
        }
        scored.truncate(n);
        scored.sort_unstable_by(by);
        let w = self.gross / n.max(1) as f64;
        Ok(Some(Targets::Weights(scored.into_iter().map(|(_, e)| (e, w)).collect())))
    }
}

/// How [`RiskParity`] sets weights.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum RiskParityMethod {
    /// Weights proportional to 1 / volatility.
    InverseVol,
    /// Equal risk contribution under the sample covariance.
    EqualRisk,
}

/// Risk parity across sleeves, from the sleeves' own unit returns known at the decision.
#[derive(Clone, Debug)]
pub struct RiskParity {
    /// The sleeves (contracts or composites).
    pub sleeves: Vec<EntityId>,
    /// Returns in the estimation window.
    pub lookback: usize,
    /// Method.
    pub method: RiskParityMethod,
    /// Total weight.
    pub gross: f64,
}

/// Equal-risk-contribution weights for covariance `cov` (row-major n × n), summing to one.
#[must_use]
pub fn equal_risk_weights(cov: &[f64], n: usize) -> Vec<f64> {
    let mut w = vec![1.0 / n as f64; n];
    for _ in 0..10_000 {
        let sw: Vec<f64> = (0..n).map(|i| (0..n).map(|j| cov[i * n + j] * w[j]).sum()).collect();
        let total: f64 = (0..n).map(|i| w[i] * sw[i]).sum();
        if total <= 0.0 {
            break;
        }
        let rc: Vec<f64> = (0..n).map(|i| w[i] * sw[i] / total).collect();
        let worst = rc.iter().map(|r| (r - 1.0 / n as f64).abs()).fold(0.0, f64::max);
        if worst < 1e-12 {
            break;
        }
        for i in 0..n {
            w[i] *= ((1.0 / n as f64) / rc[i].max(1e-300)).sqrt();
        }
        let s: f64 = w.iter().sum();
        w.iter_mut().for_each(|x| *x /= s);
    }
    w
}

impl DecisionModel for RiskParity {
    fn name(&self) -> String {
        format!("risk_parity({:?}, {})", self.method, self.lookback)
    }

    fn params(&self) -> u64 {
        let mut h = Hash64::new();
        for s in &self.sleeves {
            h.u64(u64::from(s.0));
        }
        h.u64(self.lookback as u64);
        h.u64(self.method as u64);
        h.f64(self.gross);
        h.finish()
    }

    fn decide(&self, ctx: &DecisionCtx<'_>) -> Result<Option<Targets>> {
        let rows: Vec<usize> = self
            .sleeves
            .iter()
            .map(|e| ctx.records.idx(*e).ok_or_else(|| Error::Unknown(format!("risk parity: sleeve {} has no unit records", ctx.view.ws.entities.name(*e)))))
            .collect::<Result<_>>()?;
        let series: Vec<Vec<f64>> = rows.iter().map(|&i| ctx.records.returns_known(i, ctx.k, self.lookback, ctx.t)).collect();
        let Some(m) = series.iter().map(Vec::len).min() else {
            return Ok(None);
        };
        if m < 2 {
            return Ok(None);
        }
        let n = rows.len();
        let x: Vec<&[f64]> = series.iter().map(|r| &r[r.len() - m..]).collect();
        let mean: Vec<f64> = x.iter().map(|r| r.iter().sum::<f64>() / m as f64).collect();
        let mut cov = vec![0.0; n * n];
        for i in 0..n {
            for j in 0..n {
                cov[i * n + j] = (0..m).map(|t| (x[i][t] - mean[i]) * (x[j][t] - mean[j])).sum::<f64>() / (m as f64 - 1.0);
            }
        }
        let w = match self.method {
            RiskParityMethod::InverseVol => {
                let inv: Vec<f64> = (0..n).map(|i| 1.0 / cov[i * n + i].sqrt().max(1e-12)).collect();
                let s: f64 = inv.iter().sum();
                inv.into_iter().map(|v| v / s).collect()
            }
            RiskParityMethod::EqualRisk => equal_risk_weights(&cov, n),
        };
        Ok(Some(Targets::Weights(self.sleeves.iter().zip(w).map(|(e, wi)| (*e, wi * self.gross)).collect())))
    }
}

/// Recorded targets from a series: the book of an existing backtester, booked by this ledger.
#[derive(Clone, Debug)]
pub struct Replay {
    /// Series of recorded targets (entity = contract, event = decision time).
    pub series: String,
    /// Weight or quantity column.
    pub column: String,
    /// Contracts that may be held (anything without a row at a decision is sold).
    pub universe: Vec<EntityId>,
    /// Whether the column holds quantities rather than weights.
    pub quantities: bool,
}

impl DecisionModel for Replay {
    fn name(&self) -> String {
        format!("replay({}.{})", self.series, self.column)
    }

    fn params(&self) -> u64 {
        let mut h = Hash64::new();
        h.str(&self.series);
        h.str(&self.column);
        h.u64(u64::from(self.quantities));
        h.finish()
    }

    /// A replay of observed holdings decides nothing new.
    fn intervenes(&self, ws: &Workspace) -> bool {
        !observed(ws, &self.series)
    }

    fn decide(&self, ctx: &DecisionCtx<'_>) -> Result<Option<Targets>> {
        let s = ctx.view.series(&self.series)?;
        let c = s.column(&self.column)?;
        let targets: Vec<(EntityId, f64)> = self.universe.iter().filter_map(|e| ctx.view.exact(s, c, *e).map(|h| (*e, h.value))).collect();
        if targets.is_empty() {
            return Ok(None);
        }
        Ok(Some(if self.quantities { Targets::Quantities(targets) } else { Targets::Weights(targets) }))
    }
}

/// Recorded trades as orders: at each decision time, every row of `series` at exactly that time
/// is an order for `column` units of its contract (positive buys). A holdings replay substitutes a
/// book's decisions with these (spec, change 4).
#[derive(Clone, Debug)]
pub struct Trades {
    /// Series of recorded trades (entity = contract, event = trade time).
    pub series: String,
    /// Quantity column.
    pub column: String,
    /// Contracts that may trade.
    pub universe: Vec<EntityId>,
}

impl DecisionModel for Trades {
    fn name(&self) -> String {
        format!("trades({}.{})", self.series, self.column)
    }

    fn params(&self) -> u64 {
        let mut h = Hash64::new();
        h.str(&self.series);
        h.str(&self.column);
        for e in &self.universe {
            h.u64(u64::from(e.0));
        }
        h.finish()
    }

    /// A replay of observed trades decides nothing new.
    fn intervenes(&self, ws: &Workspace) -> bool {
        !observed(ws, &self.series)
    }

    fn decide(&self, ctx: &DecisionCtx<'_>) -> Result<Option<Targets>> {
        let s = ctx.view.series(&self.series)?;
        let c = s.column(&self.column)?;
        let orders: Vec<(EntityId, f64)> =
            self.universe.iter().filter_map(|e| ctx.view.exact(s, c, *e).map(|h| (*e, h.value))).filter(|(_, q)| *q != 0.0 && q.is_finite()).collect();
        Ok((!orders.is_empty()).then_some(Targets::Orders(orders)))
    }
}

/// An intervention from a time: `before` decides until `at`, `after` from then on. A branch
/// whose `after` is `before` reproduces the run without it.
#[derive(Clone)]
pub struct Branch {
    /// When the branch starts.
    pub at: Ts,
    /// The decisions before.
    pub before: Arc<dyn DecisionModel>,
    /// The decisions from `at`.
    pub after: Arc<dyn DecisionModel>,
}

impl DecisionModel for Branch {
    fn name(&self) -> String {
        format!("branch(at={}, before={}, after={})", self.at, self.before.name(), self.after.name())
    }

    fn params(&self) -> u64 {
        let mut h = Hash64::new();
        h.i64(self.at.0);
        h.u64(self.before.params());
        h.u64(self.after.params());
        h.finish()
    }

    /// A branch intervenes if either side does, or if the decisions from `at` differ from those
    /// before: a branch with no change is the run without it.
    fn intervenes(&self, ws: &Workspace) -> bool {
        let same = self.before.name() == self.after.name() && self.before.params() == self.after.params();
        self.before.intervenes(ws) || self.after.intervenes(ws) || !same
    }

    fn decide(&self, ctx: &DecisionCtx<'_>) -> Result<Option<Targets>> {
        if ctx.t < self.at { self.before.decide(ctx) } else { self.after.decide(ctx) }
    }
}

/// A deal manager's reinvestment: while the deal publishes `reinvesting`, spend the spendable
/// principal cash on `into`, in equal value. Nothing is sold.
#[derive(Clone, Debug)]
pub struct Reinvest {
    /// Eligible collateral, bought in equal value.
    pub into: Vec<EntityId>,
    /// Cash left unspent.
    pub keep: f64,
}

impl DecisionModel for Reinvest {
    fn name(&self) -> String {
        format!("reinvest({} contracts)", self.into.len())
    }

    fn params(&self) -> u64 {
        let mut h = Hash64::new();
        for e in &self.into {
            h.u64(u64::from(e.0));
        }
        h.f64(self.keep);
        h.finish()
    }

    fn decide(&self, ctx: &DecisionCtx<'_>) -> Result<Option<Targets>> {
        if ctx.state("reinvesting") != Some(1.0) || self.into.is_empty() {
            return Ok(None);
        }
        let cash = ctx.cash.first().map_or(0.0, |(_, c)| *c) - self.keep;
        if cash <= 1e-9 {
            return Ok(None);
        }
        let each = cash / self.into.len() as f64;
        let mut orders = Vec::with_capacity(self.into.len());
        for e in &self.into {
            let Some(i) = ctx.records.idx(*e) else {
                return Err(Error::Unknown(format!("reinvestment into {}, which has no contract", ctx.view.ws.entities.name(*e))));
            };
            let v = ctx.records.value[ctx.records.at(i, ctx.k)];
            if v.is_finite() && v > 0.0 {
                orders.push((*e, each / v));
            }
        }
        Ok((!orders.is_empty()).then_some(Targets::Orders(orders)))
    }
}

/// A forced seller's mandate: at each decision, offer the whole remaining economic position of
/// every held contract whose latest known value of `series.column` is above `above`. For
/// example, a rating notch past an index's floor (a fallen angel), with the scale declared by
/// whoever maps the ratings to notches. Information not yet known, or no value at all, triggers
/// nothing.
#[derive(Clone, Debug)]
pub struct ExitAbove {
    /// The series of values (for example rating notches, keyed by contract).
    pub series: String,
    /// The column.
    pub column: String,
    /// Exit above this.
    pub above: f64,
}

impl DecisionModel for ExitAbove {
    fn name(&self) -> String {
        format!("exit above {} on {}.{}", self.above, self.series, self.column)
    }

    fn params(&self) -> u64 {
        let mut h = Hash64::new();
        h.str(&self.series);
        h.str(&self.column);
        h.f64(self.above);
        h.finish()
    }

    fn decide(&self, ctx: &DecisionCtx<'_>) -> Result<Option<Targets>> {
        let s = ctx.view.series(&self.series)?;
        let c = s.column(&self.column)?;
        let mut orders = Vec::new();
        for &(i, q) in ctx.holdings.iter().filter(|(_, q)| *q > 0.0) {
            let e = ctx.records.entities()[i];
            let name = ctx.view.ws.entities.name(e);
            // The journal records what each decision knew, and so why it acted or did not.
            match ctx.view.asof(s, c, e) {
                Some(h) if h.value > self.above => {
                    ctx.note(format!("{name}: {} = {} (effective {}, known {}) is above {}: offer {q}", self.column, h.value, h.event, h.known, self.above));
                    orders.push((e, -q));
                }
                Some(h) => ctx.note(format!("{name}: {} = {} (effective {}, known {}): keep", self.column, h.value, h.event, h.known)),
                None => ctx.note(format!("{name}: no {} known: keep", self.column)),
            }
        }
        Ok((!orders.is_empty()).then_some(Targets::Orders(orders)))
    }
}

#[cfg(test)]
mod tests {
    use super::equal_risk_weights;

    #[test]
    fn equal_risk_contributions() {
        let cov = [0.04, 0.006, 0.0, 0.006, 0.09, 0.012, 0.0, 0.012, 0.01];
        let w = equal_risk_weights(&cov, 3);
        let sw: Vec<f64> = (0..3).map(|i| (0..3).map(|j| cov[i * 3 + j] * w[j]).sum()).collect();
        let rc: Vec<f64> = (0..3).map(|i| w[i] * sw[i]).collect();
        for r in &rc {
            assert!((r - rc[0]).abs() < 1e-10);
        }
        assert!((w.iter().sum::<f64>() - 1.0).abs() < 1e-12);
    }
}
