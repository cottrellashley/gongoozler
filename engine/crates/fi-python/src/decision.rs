//! Decisions: built in (top N, risk parity, replay, trades, reinvestment, rolls, the optimiser), branched, or Python callables (`fi.Decision`).

use super::*;

/// A decision model, and where the Python callables in it keep their exceptions.
pub(crate) type BuiltDecision = (Arc<dyn DecisionModel>, Vec<Arc<Errors>>);

pub(crate) enum DecisionSpec {
    TopN { series: String, column: String, universe: Option<Vec<String>>, n: usize, gross: f64 },
    RiskParity { sleeves: Vec<String>, lookback: usize, method: RiskParityMethod, gross: f64 },
    Replay { series: String, column: String, universe: Option<Vec<String>>, quantities: bool },
    Trades { series: String, column: String, universe: Option<Vec<String>> },
    Reinvest { into: Vec<String>, keep: f64 },
    Roll { chain: Vec<(String, Ts)>, contracts: f64, days_before: i64 },
    Optimise { spec: Box<fi_std::Optimise>, universe: Option<Vec<String>> },
    Branch { at: Ts, before: Py<PyDecisionSpec>, after: Py<PyDecisionSpec> },
    Python { decide: Py<PyAny>, name: String, params: u64 },
}

/// What a strategy decides at each scheduled time: built in, or a Python callable.
#[pyclass(name = "Decision", module = "fi_engine._native", frozen)]
pub struct PyDecisionSpec {
    pub(crate) spec: DecisionSpec,
}

#[pymethods]
impl PyDecisionSpec {
    /// Equal weights in the top `n` contracts by `series.column` known at the decision.
    #[staticmethod]
    #[pyo3(signature = (signal, n, *, universe = None, gross = 1.0))]
    fn top_n(signal: &str, n: usize, universe: Option<Vec<String>>, gross: f64) -> PyResult<Self> {
        let (s, c) = split(signal)?;
        Ok(Self { spec: DecisionSpec::TopN { series: s.into(), column: c.into(), universe, n, gross } })
    }

    /// Risk parity across sleeves (contracts or composites) from their own returns known at the
    /// decision: `method` is `"inverse_vol"` or `"equal_risk"`.
    #[staticmethod]
    #[pyo3(signature = (sleeves, lookback, *, method = "inverse_vol", gross = 1.0))]
    fn risk_parity(sleeves: Vec<String>, lookback: usize, method: &str, gross: f64) -> PyResult<Self> {
        let method = match method {
            "inverse_vol" => RiskParityMethod::InverseVol,
            "equal_risk" => RiskParityMethod::EqualRisk,
            m => return Err(PyValueError::new_err(format!("method is inverse_vol or equal_risk, not {m}"))),
        };
        Ok(Self { spec: DecisionSpec::RiskParity { sleeves, lookback, method, gross } })
    }

    /// Recorded weights (or quantities) from `series.column`: an existing backtester's book,
    /// booked by this ledger. Contracts without a row at a decision are sold.
    #[staticmethod]
    #[pyo3(signature = (targets, *, universe = None, quantities = false))]
    fn replay(targets: &str, universe: Option<Vec<String>>, quantities: bool) -> PyResult<Self> {
        let (s, c) = split(targets)?;
        Ok(Self { spec: DecisionSpec::Replay { series: s.into(), column: c.into(), universe, quantities } })
    }

    /// Recorded trades as orders: every row of `series.column` at a decision time is an order
    /// for that many units of its contract (positive buys). A holdings replay.
    #[staticmethod]
    #[pyo3(signature = (trades, *, universe = None))]
    fn trades(trades: &str, universe: Option<Vec<String>>) -> PyResult<Self> {
        let (s, c) = split(trades)?;
        Ok(Self { spec: DecisionSpec::Trades { series: s.into(), column: c.into(), universe } })
    }

    /// A deal manager's reinvestment: while the deal publishes `reinvesting`, spend the
    /// principal cash (less `keep`) on `into`, in equal value.
    #[staticmethod]
    #[pyo3(signature = (into, *, keep = 0.0))]
    fn reinvest(into: Vec<String>, keep: f64) -> Self {
        Self { spec: DecisionSpec::Reinvest { into, keep } }
    }

    /// A futures roll: hold `contracts` units of the first contract of `chain` (pairs of a
    /// contract and its expiration) more than `days_before` days from expiry, and none of the
    /// others. Negative `contracts` hold a short.
    #[staticmethod]
    #[pyo3(signature = (chain, contracts, *, days_before = 5))]
    fn roll(chain: Vec<(String, Bound<'_, PyAny>)>, contracts: f64, days_before: i64) -> PyResult<Self> {
        let chain = chain.iter().map(|(n, x)| Ok((n.clone(), convert::ts(x)?))).collect::<PyResult<_>>()?;
        Ok(Self { spec: DecisionSpec::Roll { chain, contracts, days_before } })
    }

    /// An optimiser: at each decision, the weights maximising `alpha` (`series.column`, expected
    /// return) less `risk_aversion` × Σ (`risk` × w)² (`risk` a `series.column` volatility, or one
    /// each) less `turnover` × Σ (w − w now)², with each weight in `bounds`, the weights summing to
    /// `gross`, exposure bands (`exposures`: `{"series.column": (min, max)}` on Σ w × x, such as a
    /// duration) and a cap on each group's weight (`groups`: `("series.column", cap)`). The
    /// solver's status and the constraints that bind go to the journal. When no weights meet the
    /// constraints, `on_infeasible` keeps the book (`"keep"`, said in the journal) or stops the
    /// run (`"fail"`): nothing is relaxed.
    #[staticmethod]
    #[pyo3(signature = (alpha, *, risk = None, risk_aversion = 1.0, turnover = 0.0, gross = 1.0, bounds = (0.0, 1.0), exposures = None, groups = None, universe = None, on_infeasible = "keep"))]
    #[allow(clippy::too_many_arguments)]
    fn optimise(
        alpha: &str,
        risk: Option<&str>,
        risk_aversion: f64,
        turnover: f64,
        gross: f64,
        bounds: (f64, f64),
        exposures: Option<HashMap<String, (f64, f64)>>,
        groups: Option<(String, f64)>,
        universe: Option<Vec<String>>,
        on_infeasible: &str,
    ) -> PyResult<Self> {
        let pair = |x: &str| -> PyResult<(String, String)> { split(x).map(|(a, b)| (a.to_owned(), b.to_owned())) };
        let mut exp: Vec<fi_std::Exposure> = exposures
            .unwrap_or_default()
            .into_iter()
            .map(|(k, (min, max))| {
                let (series, column) = pair(&k)?;
                Ok(fi_std::Exposure { series, column, min, max })
            })
            .collect::<PyResult<_>>()?;
        exp.sort_by(|a, b| (&a.series, &a.column).cmp(&(&b.series, &b.column)));
        let groups = groups
            .map(|(g, cap)| -> PyResult<fi_std::GroupCap> {
                let (series, column) = pair(&g)?;
                Ok(fi_std::GroupCap { series, column, cap })
            })
            .transpose()?;
        let on_infeasible = match on_infeasible {
            "keep" => fi_std::OnInfeasible::Keep,
            "fail" => fi_std::OnInfeasible::Fail,
            o => return Err(PyValueError::new_err(format!("on_infeasible is keep or fail, not {o}"))),
        };
        if bounds.0.is_nan() || bounds.1.is_nan() || bounds.0 > bounds.1 {
            return Err(PyValueError::new_err(format!("bounds {bounds:?}: the least above the most")));
        }
        let spec = fi_std::Optimise {
            alpha: pair(alpha)?,
            risk: risk.map(pair).transpose()?,
            risk_aversion,
            turnover,
            gross,
            bounds,
            exposures: exp,
            groups,
            universe: Vec::new(),
            on_infeasible,
        };
        Ok(Self { spec: DecisionSpec::Optimise { spec: Box::new(spec), universe } })
    }

    /// An intervention from a time: `before` decides until `at`, `after` from then on. A branch
    /// whose `after` is `before` reproduces the run without it.
    #[staticmethod]
    fn branch(at: &Bound<'_, PyAny>, before: Py<PyDecisionSpec>, after: Py<PyDecisionSpec>) -> PyResult<Self> {
        Ok(Self { spec: DecisionSpec::Branch { at: convert::ts(at)?, before, after } })
    }

    /// A Python decision: `decide(ctx)` returns `None` (keep the book), a mapping of contract to
    /// weight, or `("weights" | "quantities", mapping)`.
    #[staticmethod]
    #[pyo3(signature = (decide, *, name = None, version = None))]
    fn python(decide: &Bound<'_, PyAny>, name: Option<String>, version: Option<u64>) -> PyResult<Self> {
        let name = match name {
            Some(n) => n,
            None => decide.getattr("__name__").and_then(|n| n.extract()).unwrap_or_else(|_| "python".to_owned()),
        };
        Ok(Self { spec: DecisionSpec::Python { decide: decide.clone().unbind(), name, params: version.unwrap_or_else(bridge::fresh) } })
    }

    fn __repr__(&self) -> String {
        match &self.spec {
            DecisionSpec::TopN { series, column, n, .. } => format!("Decision.top_n({series}.{column}, {n})"),
            DecisionSpec::RiskParity { sleeves, lookback, method, .. } => format!("Decision.risk_parity({} sleeves, {lookback}, {method:?})", sleeves.len()),
            DecisionSpec::Replay { series, column, quantities, .. } => format!("Decision.replay({series}.{column}, quantities={quantities})"),
            DecisionSpec::Trades { series, column, .. } => format!("Decision.trades({series}.{column})"),
            DecisionSpec::Reinvest { into, .. } => format!("Decision.reinvest({into:?})"),
            DecisionSpec::Roll { chain, contracts, days_before } => format!("Decision.roll({} contracts, {contracts}, days_before={days_before})", chain.len()),
            DecisionSpec::Optimise { spec, .. } => format!("Decision.optimise({}.{})", spec.alpha.0, spec.alpha.1),
            DecisionSpec::Branch { at, before, after } => format!("Decision.branch({at}, {}, {})", before.get().__repr__(), after.get().__repr__()),
            DecisionSpec::Python { name, .. } => format!("Decision.python({name})"),
        }
    }
}

impl PyDecisionSpec {
    pub(crate) fn name(&self) -> String {
        match &self.spec {
            DecisionSpec::TopN { series, column, n, .. } => format!("top{n}({series}.{column})"),
            DecisionSpec::RiskParity { .. } => "risk_parity".into(),
            DecisionSpec::Replay { series, column, .. } => format!("replay({series}.{column})"),
            DecisionSpec::Trades { series, column, .. } => format!("trades({series}.{column})"),
            DecisionSpec::Reinvest { .. } => "reinvest".into(),
            DecisionSpec::Roll { .. } => "roll".into(),
            DecisionSpec::Optimise { spec, .. } => format!("optimise({}.{})", spec.alpha.0, spec.alpha.1),
            DecisionSpec::Branch { at, before, after } => format!("branch({at}: {} then {})", before.get().name(), after.get().name()),
            DecisionSpec::Python { name, .. } => name.clone(),
        }
    }

    pub(crate) fn build(&self, py: Python<'_>, names: &Arc<Names>) -> PyResult<BuiltDecision> {
        let resolve = |u: &Option<Vec<String>>| -> PyResult<Vec<EntityId>> {
            match u {
                None => Ok(names.ids.clone()),
                Some(v) => v.iter().map(|n| names.id(n)).collect(),
            }
        };
        Ok(match &self.spec {
            DecisionSpec::TopN { series, column, universe, n, gross } => {
                (Arc::new(TopN { series: series.clone(), column: column.clone(), universe: resolve(universe)?, n: *n, gross: *gross }), Vec::new())
            }
            DecisionSpec::RiskParity { sleeves, lookback, method, gross } => {
                let sleeves = sleeves.iter().map(|s| names.id(s)).collect::<PyResult<_>>()?;
                (Arc::new(RiskParity { sleeves, lookback: *lookback, method: *method, gross: *gross }), Vec::new())
            }
            DecisionSpec::Replay { series, column, universe, quantities } => {
                (Arc::new(Replay { series: series.clone(), column: column.clone(), universe: resolve(universe)?, quantities: *quantities }), Vec::new())
            }
            DecisionSpec::Trades { series, column, universe } => {
                (Arc::new(Trades { series: series.clone(), column: column.clone(), universe: resolve(universe)? }), Vec::new())
            }
            DecisionSpec::Reinvest { into, keep } => {
                let into = into.iter().map(|n| names.id(n)).collect::<PyResult<_>>()?;
                (Arc::new(Reinvest { into, keep: *keep }), Vec::new())
            }
            DecisionSpec::Optimise { spec, universe } => {
                let mut o = (**spec).clone();
                o.universe = resolve(universe)?;
                (Arc::new(o), Vec::new())
            }
            DecisionSpec::Roll { chain, contracts, days_before } => {
                let chain = chain.iter().map(|(n, x)| Ok((names.id(n)?, *x))).collect::<PyResult<_>>()?;
                (Arc::new(fi_exec::Roll { chain, contracts: *contracts, days_before: *days_before }), Vec::new())
            }
            DecisionSpec::Branch { at, before, after } => {
                let (b, mut e) = before.get().build(py, names)?;
                let (a, e2) = after.get().build(py, names)?;
                e.extend(e2);
                (Arc::new(Branch { at: *at, before: b, after: a }), e)
            }
            DecisionSpec::Python { decide, name, params } => {
                let errors = Arc::new(Errors::default());
                let d = PyDecision { name: name.clone(), params: *params, decide: decide.clone_ref(py), contracts: names.clone(), errors: errors.clone() };
                (Arc::new(d), vec![errors])
            }
        })
    }
}
