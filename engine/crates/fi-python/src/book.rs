//! Books from Python (spec, changes 1, 2 and 5): policies with their clocks, scopes and
//! priorities; an execution model; and every assumption about cash, currencies, settlement,
//! capital flows, mandate rules and the horizon, named.

use std::sync::Arc;

use numpy::PyArray1;
use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyDict;

use fi_exec::{Capacity, PortfolioTrade, RfqFill, RfqProcess};
use fi_kernel::{
    Advance, Agreement, AgreementKind, Basis, Breach, CapitalFlows, CashPolicy, CashRate, Constraint, ConstraintScope, DayCount, EachOrder, EntityId,
    Execution, Face, FlowCtx, FlowModel, FromSeries, FxConversion, HoldingPolicy, Horizon, LedgerConfig, Measure, OpeningDue, Phase, Response, Rounding,
    RoundingMode, Routing, Settlement, ShortPolicy, StepOrder, Ts, Value, Workspace,
};

/// A rounding term: `(unit, mode)`, the unit a number (0.01) or a currency code (`"USD"`,
/// whose minor unit Nautilus's currency registry gives), the mode `"down"`, `"up"`, `"half_up"`
/// or `"half_even"`.
pub(crate) fn parse_rounding(obj: Option<&Bound<'_, PyAny>>) -> PyResult<Option<Rounding>> {
    let Some(obj) = obj.filter(|o| !o.is_none()) else { return Ok(None) };
    let (unit, mode): (Bound<'_, PyAny>, String) =
        obj.extract().map_err(|_| PyTypeError::new_err("a rounding term is (unit, mode): a unit or a currency code, and down, up, half_up or half_even"))?;
    let mode = match mode.as_str() {
        "down" => RoundingMode::Down,
        "up" => RoundingMode::Up,
        "half_up" => RoundingMode::HalfUp,
        "half_even" => RoundingMode::HalfEven,
        m => return Err(PyValueError::new_err(format!("a rounding mode is down, up, half_up or half_even, not {m}"))),
    };
    if let Ok(code) = unit.extract::<String>() {
        return fi_exec::rounding(&code, mode)
            .map(Some)
            .map_err(|_| PyValueError::new_err(format!("Nautilus's currency registry has no {code}: give the unit as a number, (0.001, \"{mode:?}\")")));
    }
    let unit: f64 = unit.extract()?;
    if unit.is_nan() || unit <= 0.0 {
        return Err(PyValueError::new_err("a rounding unit is positive"));
    }
    Ok(Some(Rounding { unit, mode }))
}
use fi_research::{Book, Executor, PolicySpec};

use crate::bridge::{Errors, Names};
use crate::{PyDecisionSpec, PyFill, PyRecords, convert, split, to_py};

/// When a policy decides.
pub(crate) enum Schedule {
    Times(Vec<Ts>),
    MonthEnds,
    Every(usize),
    All,
}

impl Schedule {
    pub(crate) fn parse(s: Option<&Bound<'_, PyAny>>) -> PyResult<Self> {
        let Some(s) = s.filter(|s| !s.is_none()) else { return Ok(Self::MonthEnds) };
        if let Ok(name) = s.extract::<String>() {
            return match name.as_str() {
                "month_ends" => Ok(Self::MonthEnds),
                "all" => Ok(Self::All),
                _ => Err(PyValueError::new_err(format!("schedule is month_ends, all, an integer or a list of times, not {name}"))),
            };
        }
        if let Ok(n) = s.extract::<usize>() {
            return if n > 0 { Ok(Self::Every(n)) } else { Err(PyValueError::new_err("schedule every 0 times")) };
        }
        Ok(Self::Times(s.try_iter()?.map(|t| convert::ts(&t?)).collect::<PyResult<_>>()?))
    }

    pub(crate) fn times(&self, cal: &fi_kernel::Calendar) -> Vec<Ts> {
        match self {
            Self::Times(t) => t.clone(),
            Self::MonthEnds => cal.month_ends(),
            Self::Every(n) => cal.every(*n, 0),
            Self::All => cal.times().to_vec(),
        }
    }
}

/// One policy of a book: a decision, when it decides, the contracts it manages, its priority, the
/// steps from decision to submission, and how long its orders work.
#[pyclass(name = "Policy", module = "fi_engine._native", frozen)]
pub struct PyPolicy {
    name: Option<String>,
    decide: Py<PyDecisionSpec>,
    schedule: Schedule,
    scope: Option<Vec<String>>,
    priority: i32,
    lag: usize,
    tif: Option<usize>,
}

#[pymethods]
impl PyPolicy {
    /// `schedule` is `"month_ends"` (default), `"all"`, an integer (every n-th time) or a list of
    /// times. `scope` lists the contracts the policy manages (default: the whole book); held
    /// contracts in scope that its targets omit are sold, others are never touched. Higher
    /// `priority` decides first. `lag` is the steps from decision to submission;
    /// `time_in_force` the steps an order works (default: the book's).
    #[new]
    #[pyo3(signature = (decide, *, schedule = None, scope = None, priority = 0, lag = 1, time_in_force = None, name = None))]
    fn new(
        decide: Py<PyDecisionSpec>,
        schedule: Option<&Bound<'_, PyAny>>,
        scope: Option<Vec<String>>,
        priority: i32,
        lag: usize,
        time_in_force: Option<usize>,
        name: Option<String>,
    ) -> PyResult<Self> {
        if time_in_force == Some(0) {
            return Err(PyValueError::new_err("an order works for at least one step"));
        }
        Ok(Self { name, decide, schedule: Schedule::parse(schedule)?, scope, priority, lag, tif: time_in_force })
    }

    /// The policy's name.
    #[getter]
    fn name(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.decide.get().name())
    }

    fn __repr__(&self) -> String {
        format!("Policy({}, priority={}, lag={})", self.name(), self.priority, self.lag)
    }
}

impl PyPolicy {
    /// The decision model, on contracts named by `names`.
    pub(crate) fn decision(&self, py: Python<'_>, names: &std::sync::Arc<Names>) -> PyResult<crate::BuiltDecision> {
        self.decide.get().build(py, names)
    }

    pub(crate) fn policy_name(&self) -> String {
        self.name()
    }

    pub(crate) fn times(&self, cal: &fi_kernel::Calendar) -> Vec<Ts> {
        self.schedule.times(cal)
    }

    pub(crate) fn lag(&self) -> usize {
        self.lag
    }
}

enum ExecSpec {
    Fill(Py<PyFill>),
    Rfq(RfqProcess),
    Portfolio(PortfolioTrade),
    Capacity(Py<PyExecution>, f64),
}

/// An execution process: orders persist, are worked each step, and fill in part, in full or not
/// at all.
#[pyclass(name = "Execution", module = "fi_engine._native", frozen)]
pub struct PyExecution {
    spec: ExecSpec,
}

#[pymethods]
impl PyExecution {
    /// A per-order fill model (research, RFQ panel, Nautilus book) as a process.
    #[staticmethod]
    fn fill(fill: Py<PyFill>) -> Self {
        Self { spec: ExecSpec::Fill(fill) }
    }

    /// Request for quote, asked again each step an order works: no answer leaves it working
    /// until it expires; `max_size` fills part per attempt; the first request comes `latency`
    /// steps after submission.
    #[staticmethod]
    #[pyo3(signature = (*, dealers = 5, response = 0.8, half_spread_bp = 5.0, dispersion_bp = 3.0, max_size = f64::INFINITY, seed = 0, latency = 0))]
    #[allow(clippy::too_many_arguments)]
    fn rfq(dealers: usize, response: f64, half_spread_bp: f64, dispersion_bp: f64, max_size: f64, seed: u64, latency: usize) -> Self {
        Self { spec: ExecSpec::Rfq(RfqProcess { panel: RfqFill { dealers, response, half_spread_bp, dispersion_bp, max_size, seed }, latency }) }
    }

    /// A portfolio trade: the orders a policy submits together are quoted as one basket, at
    /// `half_spread_bp + impact_bp × √(notional ÷ 1e6)`, and accepted whole with probability
    /// `acceptance` (a rejected basket keeps working until it expires).
    #[staticmethod]
    #[pyo3(signature = (*, half_spread_bp = 3.0, impact_bp = 1.0, acceptance = 0.9, seed = 0, latency = 0))]
    fn portfolio(half_spread_bp: f64, impact_bp: f64, acceptance: f64, seed: u64, latency: usize) -> Self {
        Self { spec: ExecSpec::Portfolio(PortfolioTrade { half_spread_bp, impact_bp, acceptance, seed, latency }) }
    }

    /// At most `max_value` per contract per step, across every order of every book run together,
    /// allocated pro rata.
    #[staticmethod]
    fn capacity(inner: Py<PyExecution>, max_value: f64) -> Self {
        Self { spec: ExecSpec::Capacity(inner, max_value) }
    }

    fn __repr__(&self) -> String {
        match &self.spec {
            ExecSpec::Fill(_) => "Execution.fill(...)".into(),
            ExecSpec::Rfq(r) => format!("Execution.{}", r.name()),
            ExecSpec::Portfolio(p) => format!("Execution.{}", p.name()),
            ExecSpec::Capacity(_, m) => format!("Execution.capacity(max_value={m})"),
        }
    }
}

impl PyExecution {
    pub(crate) fn build(&self, names: &Names) -> PyResult<Arc<dyn Execution>> {
        Ok(match &self.spec {
            ExecSpec::Fill(f) => Arc::new(EachOrder(f.get().build(names)?)),
            ExecSpec::Rfq(r) => Arc::new(r.clone()),
            ExecSpec::Portfolio(p) => Arc::new(p.clone()),
            ExecSpec::Capacity(inner, max) => Arc::new(Capacity { inner: inner.get().build(names)?, max_value: *max }),
        })
    }
}

#[derive(Clone)]
enum MeasureSpec {
    Value,
    Face,
    Series(String, String),
}

#[derive(Clone)]
enum ScopeSpec {
    Book,
    Contracts(Vec<String>),
    PerGroup(String, String),
}

/// A mandate rule on the book's exposure, checked after fills and funding at every time. Hard
/// rules are never relaxed; soft ones carry a penalty; a mandatory rule must be hard.
#[pyclass(name = "Constraint", module = "fi_engine._native", frozen)]
pub struct PyConstraint {
    name: String,
    measure: MeasureSpec,
    scope: ScopeSpec,
    min: Option<f64>,
    max: Option<f64>,
    relative: bool,
    hard: bool,
    mandatory: bool,
    penalty: Option<f64>,
    fail: bool,
}

fn measure_spec(measure: &str) -> PyResult<MeasureSpec> {
    Ok(match measure {
        "value" => MeasureSpec::Value,
        "face" => MeasureSpec::Face,
        spec => {
            let (s, c) = split(spec)?;
            MeasureSpec::Series(s.into(), c.into())
        }
    })
}

fn scope_spec(contracts: Option<Vec<String>>, per: Option<&str>) -> PyResult<ScopeSpec> {
    Ok(match (contracts, per) {
        (Some(_), Some(_)) => return Err(PyValueError::new_err("a constraint covers contracts or groups, not both")),
        (Some(c), None) => ScopeSpec::Contracts(c),
        (None, Some(p)) => {
            let (s, c) = split(p)?;
            ScopeSpec::PerGroup(s.into(), c.into())
        }
        (None, None) => ScopeSpec::Book,
    })
}

impl PyConstraint {
    fn kernel(&self, measure: Arc<dyn Measure>, scope: ConstraintScope) -> Constraint {
        Constraint {
            name: self.name.clone(),
            measure,
            scope,
            min: self.min,
            max: self.max,
            relative: self.relative,
            hard: self.hard,
            mandatory: self.mandatory,
            penalty: self.penalty,
            fail: self.fail,
        }
    }

    fn checked(self) -> PyResult<Self> {
        self.kernel(Arc::new(Value), ConstraintScope::Book).validate().map_err(to_py)?;
        Ok(self)
    }

    pub(crate) fn build(&self, ws: &Workspace, names: &Names) -> PyResult<Constraint> {
        let measure: Arc<dyn Measure> = match &self.measure {
            MeasureSpec::Value => Arc::new(Value),
            MeasureSpec::Face => Arc::new(Face),
            MeasureSpec::Series(s, c) => {
                ws.series(s).and_then(|x| x.column(c)).map_err(to_py)?;
                Arc::new(FromSeries { series: s.clone(), column: c.clone() })
            }
        };
        let scope = match &self.scope {
            ScopeSpec::Book => ConstraintScope::Book,
            ScopeSpec::Contracts(c) => ConstraintScope::Contracts(c.iter().map(|n| names.id(n)).collect::<PyResult<_>>()?),
            ScopeSpec::PerGroup(s, c) => {
                ws.series(s).and_then(|x| x.column(c)).map_err(to_py)?;
                ConstraintScope::PerGroup { series: s.clone(), column: c.clone() }
            }
        };
        Ok(self.kernel(measure, scope))
    }
}

#[pymethods]
impl PyConstraint {
    /// A hard rule: `measure` is `"value"`, `"face"` or `"series.column"` (a per-unit measure);
    /// it covers the whole book, `contracts`, or each group `per="series.column"` (an issuer, a
    /// sector) separately; `relative` bounds are fractions of NAV. A `mandatory` rule can never
    /// become a penalty. `fail` makes a breach fail the run instead of being reported.
    #[staticmethod]
    #[pyo3(signature = (name, *, measure = "value", contracts = None, per = None, min = None, max = None, relative = false, mandatory = false, fail = false))]
    #[allow(clippy::too_many_arguments)]
    fn hard(
        name: &str,
        measure: &str,
        contracts: Option<Vec<String>>,
        per: Option<&str>,
        min: Option<f64>,
        max: Option<f64>,
        relative: bool,
        mandatory: bool,
        fail: bool,
    ) -> PyResult<Self> {
        Self {
            name: name.into(),
            measure: measure_spec(measure)?,
            scope: scope_spec(contracts, per)?,
            min,
            max,
            relative,
            hard: true,
            mandatory,
            penalty: None,
            fail,
        }
        .checked()
    }

    /// A soft preference, with a positive `penalty` per unit of excess. A mandatory rule cannot be
    /// soft.
    #[staticmethod]
    #[pyo3(signature = (name, *, penalty, measure = "value", contracts = None, per = None, min = None, max = None, relative = false, mandatory = false))]
    #[allow(clippy::too_many_arguments)]
    fn soft(
        name: &str,
        penalty: f64,
        measure: &str,
        contracts: Option<Vec<String>>,
        per: Option<&str>,
        min: Option<f64>,
        max: Option<f64>,
        relative: bool,
        mandatory: bool,
    ) -> PyResult<Self> {
        Self {
            name: name.into(),
            measure: measure_spec(measure)?,
            scope: scope_spec(contracts, per)?,
            min,
            max,
            relative,
            hard: false,
            mandatory,
            penalty: Some(penalty),
            fail: false,
        }
        .checked()
    }

    fn __repr__(&self) -> String {
        format!("Constraint({}, hard={}, min={:?}, max={:?})", self.name, self.hard, self.min, self.max)
    }
}

/// A Python capital-flow model: `flow(ctx)` returns the subscription (positive) or redemption
/// now, from the book's own history up to the previous time (`ctx["nav"]`, `ctx["nav_per_unit"]`).
struct PyFlow {
    callable: Py<PyAny>,
    errors: Arc<Errors>,
}

impl FlowModel for PyFlow {
    fn name(&self) -> String {
        "python flows".into()
    }

    fn flow(&self, ctx: &FlowCtx<'_>) -> fi_kernel::Result<f64> {
        Python::attach(|py| {
            let r = (|| -> PyResult<f64> {
                let d = PyDict::new(py);
                d.set_item("t", ctx.t.0)?;
                d.set_item("k", ctx.k)?;
                d.set_item("date", convert::date(py, ctx.t)?)?;
                d.set_item("nav", PyArray1::from_slice(py, ctx.nav))?;
                d.set_item("nav_per_unit", PyArray1::from_slice(py, ctx.nav_per_unit))?;
                self.callable.bind(py).call1((d,))?.extract::<f64>()
            })();
            r.map_err(|e| self.errors.fail(e, "python flow model"))
        })
    }
}

enum CashSpec {
    Forbid(Breach),
    Borrow { limit: f64, rate: Option<String> },
    Unlimited,
}

enum ShortSpec {
    Forbid,
    Borrow { fee_bp: f64, available: Option<String> },
    Unlimited,
}

enum FlowSpec {
    Series { series: String, column: String, settle: usize },
    Model { callable: Py<PyAny>, settle: usize },
}

enum ExecChoice {
    Fill(Py<PyFill>),
    Process(Py<PyExecution>),
}

/// A book: its policies, how its orders execute, and its settings.
#[pyclass(name = "Book", module = "fi_engine._native", frozen)]
pub struct PyBook {
    name: Option<String>,
    policies: Vec<Py<PyPolicy>>,
    execution: ExecChoice,
    initial_cash: f64,
    holding: HoldingPolicy,
    cash: CashSpec,
    /// Deposit rates: (currency, `series.column`, day count); no currency is the base.
    deposit: Vec<(Option<String>, String, DayCount)>,
    day_count: DayCount,
    shorts: ShortSpec,
    currency: Option<String>,
    fx: Option<String>,
    conversion: FxConversion,
    settlement: (usize, Vec<(String, usize)>),
    horizon: Horizon,
    flows: Option<FlowSpec>,
    tif: usize,
    constraints: Vec<Py<PyConstraint>>,
    routing: Routing,
    journal: bool,
    keep_holdings: bool,
    opening: Vec<(String, f64)>,
    opening_dues: Vec<(f64, Ts, Option<String>)>,
    financing: Vec<Py<PyAgreement>>,
    responses: Vec<Response>,
    order: StepOrder,
}

enum AgreementSpec {
    Secured {
        collateral: Vec<(String, Option<String>, f64)>,
        basis: Basis,
        concentration: Option<f64>,
        limit: f64,
        rate: Option<String>,
        maturity: Option<Ts>,
        cure: usize,
        opening: f64,
        draw: bool,
        sweep: bool,
        margin: Option<Vec<Ts>>,
        rounding: Option<Rounding>,
        day_count: DayCount,
    },
    Csa {
        set: Vec<String>,
        threshold: f64,
        minimum: f64,
        every: usize,
    },
}

/// A financing agreement between the book and a counterparty (spec, change 5): secured
/// borrowing (a repo, a warehouse, a borrowing base) or a credit support annex.
#[pyclass(name = "Agreement", module = "fi_engine._native", frozen)]
pub struct PyAgreement {
    name: String,
    spec: AgreementSpec,
}

#[pymethods]
impl PyAgreement {
    /// Secured borrowing against `collateral`, `{contract: advance}`: the fraction of its value
    /// lent (one less the haircut), a number or a `"series.column"` the agreement sets over time
    /// (zero when ineligible).
    ///
    /// * `basis`: `"market"` (value) or `"par"` (face).
    /// * `concentration`: no holding counts above this fraction of the aggregate eligible
    ///   collateral.
    /// * `limit`: the commitment. `rate`: `"series.column"`, Act/360, on the balance.
    /// * `maturity`: when the balance is due (nothing extends it). `cure`: steps a margin call may
    ///   stay unpaid. `opening`: the balance at the first time.
    /// * `draw`: drawn on to meet a cash need. `sweep`: repaid from free cash at each step.
    ///
    /// * `margin`: the times the lender assesses margin (its margin frequency); calls are made
    ///   only then. Default: at every step. A call due is paid from cash the book can spend then;
    ///   what it cannot pay stays called, and is an economic failure.
    /// * `rounding`: how capacity becomes whole minor units, `(unit or currency, mode)`, such as
    ///   `("USD", "down")`.
    /// * `day_count`: how its interest accrues (default ACT/360).
    ///
    /// Collateral pledged to an earlier agreement in the book's list supports no later one.
    #[staticmethod]
    #[pyo3(signature = (
        name, collateral, *, basis = "market", concentration = None, limit = f64::INFINITY, rate = None, maturity = None, cure = 0, opening = 0.0,
        draw = true, sweep = false, margin = None, rounding = None, day_count = "ACT/360"
    ))]
    #[allow(clippy::too_many_arguments)]
    fn secured(
        name: String,
        collateral: &Bound<'_, PyDict>,
        basis: &str,
        concentration: Option<f64>,
        limit: f64,
        rate: Option<String>,
        maturity: Option<&Bound<'_, PyAny>>,
        cure: usize,
        opening: f64,
        draw: bool,
        sweep: bool,
        margin: Option<Vec<Bound<'_, PyAny>>>,
        rounding: Option<&Bound<'_, PyAny>>,
        day_count: &str,
    ) -> PyResult<Self> {
        let day_count: DayCount = day_count.parse().map_err(|e: fi_terms::Error| PyValueError::new_err(e.to_string()))?;
        let margin = margin.map(|m| m.iter().map(convert::ts).collect::<PyResult<Vec<_>>>()).transpose()?.map(|mut m| {
            m.sort_unstable();
            m
        });
        let rounding = parse_rounding(rounding)?;
        let basis = match basis {
            "market" => Basis::Market,
            "par" => Basis::Par,
            b => return Err(PyValueError::new_err(format!("basis is market or par, not {b}"))),
        };
        let mut c = Vec::with_capacity(collateral.len());
        for (k, v) in collateral.iter() {
            let contract: String = k.extract()?;
            if let Ok(x) = v.extract::<f64>() {
                c.push((contract, None, x));
            } else {
                let spec: String = v.extract().map_err(|_| PyTypeError::new_err("an advance rate is a number or a \"series.column\""))?;
                split(&spec)?;
                c.push((contract, Some(spec), f64::NAN));
            }
        }
        let maturity = maturity.filter(|m| !m.is_none()).map(convert::ts).transpose()?;
        if let Some(r) = &rate {
            split(r)?;
        }
        Ok(Self {
            name,
            spec: AgreementSpec::Secured {
                collateral: c,
                basis,
                concentration,
                limit,
                rate,
                maturity,
                cure,
                opening,
                draw,
                sweep,
                margin,
                rounding,
                day_count,
            },
        })
    }

    /// A credit support annex over the netting set `contracts`: its value beyond `threshold` is
    /// collateralised in cash every `every` steps, in moves of at least `minimum`. Posting is a
    /// receivable, not a loss. Separate annexes never net.
    #[staticmethod]
    #[pyo3(signature = (name, contracts, *, threshold = 0.0, minimum = 0.0, every = 1))]
    fn csa(name: String, contracts: Vec<String>, threshold: f64, minimum: f64, every: usize) -> PyResult<Self> {
        if every == 0 {
            return Err(PyValueError::new_err("margin every 0 steps"));
        }
        Ok(Self { name, spec: AgreementSpec::Csa { set: contracts, threshold, minimum, every } })
    }

    /// The agreement's name.
    #[getter]
    fn name(&self) -> String {
        self.name.clone()
    }

    fn __repr__(&self) -> String {
        match &self.spec {
            AgreementSpec::Secured { collateral, .. } => format!("Agreement.secured({}, {} contracts)", self.name, collateral.len()),
            AgreementSpec::Csa { set, .. } => format!("Agreement.csa({}, {} contracts)", self.name, set.len()),
        }
    }
}

impl PyAgreement {
    fn build(&self, names: &Names) -> PyResult<Agreement> {
        let kind = match &self.spec {
            AgreementSpec::Secured { collateral, basis, concentration, limit, rate, maturity, cure, opening, draw, sweep, margin, rounding, day_count } => {
                AgreementKind::Secured {
                    day_count: *day_count,
                    collateral: collateral
                        .iter()
                        .map(|(n, s, x)| {
                            let adv = match s {
                                Some(s) => {
                                    let (a, b) = split(s)?;
                                    Advance::Series(a.into(), b.into())
                                }
                                None => Advance::Fixed(*x),
                            };
                            Ok((names.id(n)?, adv))
                        })
                        .collect::<PyResult<_>>()?,
                    basis: *basis,
                    concentration: *concentration,
                    limit: *limit,
                    rate: pair(rate.as_deref())?,
                    maturity: *maturity,
                    cure: *cure,
                    opening: *opening,
                    draw: *draw,
                    sweep: *sweep,
                    margin: margin.clone(),
                    rounding: *rounding,
                }
            }
            AgreementSpec::Csa { set, threshold, minimum, every } => {
                AgreementKind::Csa { set: set.iter().map(|n| names.id(n)).collect::<PyResult<_>>()?, threshold: *threshold, minimum: *minimum, every: *every }
            }
        };
        Ok(Agreement { name: self.name.clone(), kind })
    }
}

fn pair(spec: Option<&str>) -> PyResult<Option<(String, String)>> {
    spec.map(|s| split(s).map(|(a, b)| (a.to_owned(), b.to_owned()))).transpose()
}

#[pymethods]
impl PyBook {
    /// A book of `policies` (a list of `Policy`).
    ///
    /// * `execution`: a `Fill` (each order on its own) or an `Execution` process. Default:
    ///   research fills at no cost.
    /// * `cash`: `"forbid"` (the default: cash never goes below zero; a purchase it cannot pay
    ///   for is cut, counted), `"forbid_refuse"`, `"forbid_fail"`, `"borrow"` (up to
    ///   `borrow_limit`, at `borrow_rate`, a `"series.column"` rate) or `"unlimited"` (reported as
    ///   an assumption). `deposit_rate` is earned on positive cash: a `"series.column"` rate for
    ///   the base currency, or a dict by currency of `"series.column"` or
    ///   `("series.column", day_count)` (default ACT/360), so cash in each currency earns its own
    ///   rate. `day_count` is that of borrowing and borrow fees (default ACT/360).
    /// * `shorts`: `"forbid"` (the default for securities; derivatives are unaffected), `"borrow"`
    ///   (at `borrow_fee_bp`, up to `borrow_available`, a `"series.column"` of units available) or
    ///   `"unlimited"`.
    /// * `currency`: the base currency; `fx` (`"series.column"`): base units per unit of each
    ///   currency. Trades in another currency convert base cash automatically
    ///   (`auto_conversion`) at `conversion_spread_bp`.
    /// * `settlement`: steps from trade to settlement, an integer or `{"default": n, contract: n}`.
    /// * `horizon`: `"mark"` or `"liquidate"` (through the execution model, with its costs).
    /// * `flows`: `fi.flows.series(...)` or `fi.flows.model(...)`: subscriptions and redemptions,
    ///   unitised so returns are time-weighted.
    /// * `time_in_force`: steps an order works before it expires (1: immediate or cancel).
    /// * `constraints`: a list of `Constraint`.
    /// * `routing`: `"cash"` or `"collections"` (income into an interest account).
    /// * `journal`: keep every transfer with its cause; `keep_holdings`: keep the holdings at every
    ///   time.
    /// * `opening`: positions held from the first time without trading, `{contract: units}` (a
    ///   book's actual holdings at a branch). A book with opening positions may have no policies:
    ///   it holds and marks them.
    /// * `opening_dues`: receivables (positive) and payables (negative) in flight at the first
    ///   time, `[(amount, settles_at)]` or `[(amount, settles_at, currency)]`. A payable is paid
    ///   from cash the book can spend when it falls due; what it cannot pay stays owed.
    /// * `financing`: a list of `Agreement`, in priority order.
    /// * `responses`: how a cash need is met, in order: `"convert"` (other currencies from base
    ///   cash), `"draw"` (on secured agreements) and `("sell", cost_bp)` (holdings at value less
    ///   the cost; pledged collateral last, repaying its lender first). Default: convert, then
    ///   draw. What none of them covers is an economic failure.
    /// * `order`: the order of a step's phases between the contract events and the mark, as a
    ///   list of every phase once (`weight_resets`, `capital_flows`, `decisions`, `execution`,
    ///   `settlement`, `recalls`, `distribution`, `agreements`, `cash_guard`, `constraints`), with
    ///   execution after decisions. Events sharing a time resolve in this order: declare another
    ///   to test whether an ordering assumption is material. Default: that list.
    #[new]
    #[pyo3(signature = (
        policies, *, execution = None, initial_cash = 1e8, holding = "drift", cash = "forbid", borrow_limit = None, borrow_rate = None,
        deposit_rate = None, day_count = "ACT/360", shorts = "forbid", borrow_fee_bp = 0.0, borrow_available = None, currency = None, fx = None,
        conversion_spread_bp = 0.0, auto_conversion = true, settlement = None, horizon = "mark", flows = None, time_in_force = 1,
        constraints = None, routing = "cash", journal = false, keep_holdings = false, opening = None, opening_dues = None, financing = None,
        responses = None, order = None, name = None
    ))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        py: Python<'_>,
        policies: Vec<Py<PyPolicy>>,
        execution: Option<&Bound<'_, PyAny>>,
        initial_cash: f64,
        holding: &str,
        cash: &str,
        borrow_limit: Option<f64>,
        borrow_rate: Option<String>,
        deposit_rate: Option<&Bound<'_, PyAny>>,
        day_count: &str,
        shorts: &str,
        borrow_fee_bp: f64,
        borrow_available: Option<String>,
        currency: Option<String>,
        fx: Option<String>,
        conversion_spread_bp: f64,
        auto_conversion: bool,
        settlement: Option<&Bound<'_, PyAny>>,
        horizon: &str,
        flows: Option<&Bound<'_, PyAny>>,
        time_in_force: usize,
        constraints: Option<Vec<Py<PyConstraint>>>,
        routing: &str,
        journal: bool,
        keep_holdings: bool,
        opening: Option<Vec<(String, f64)>>,
        opening_dues: Option<Vec<Bound<'_, PyAny>>>,
        financing: Option<Vec<Py<PyAgreement>>>,
        responses: Option<Vec<Bound<'_, PyAny>>>,
        order: Option<Vec<String>>,
        name: Option<String>,
    ) -> PyResult<Self> {
        let dc = |x: &str| -> PyResult<DayCount> { x.parse().map_err(|e: fi_terms::Error| PyValueError::new_err(e.to_string())) };
        let day_count = dc(day_count)?;
        let deposit = match deposit_rate.filter(|d| !d.is_none()) {
            None => Vec::new(),
            Some(d) => {
                if let Ok(spec) = d.extract::<String>() {
                    vec![(None, spec, DayCount::Act360)]
                } else {
                    let d = d.cast::<PyDict>().map_err(|_| PyTypeError::new_err("deposit_rate is \"series.column\" or a dict by currency"))?;
                    let mut out = Vec::new();
                    for (k, v) in d.iter() {
                        let ccy: String = k.extract()?;
                        let (spec, count) = match v.extract::<String>() {
                            Ok(s) => (s, DayCount::Act360),
                            Err(_) => {
                                let (s, c): (String, String) =
                                    v.extract().map_err(|_| PyTypeError::new_err("a deposit rate is \"series.column\" or (\"series.column\", day_count)"))?;
                                (s, dc(&c)?)
                            }
                        };
                        out.push((Some(ccy), spec, count));
                    }
                    out.sort_by(|a, b| a.0.cmp(&b.0));
                    out
                }
            }
        };
        let order = match order {
            None => StepOrder::default(),
            Some(o) => StepOrder::new(o.iter().map(|p| p.parse::<Phase>()).collect::<fi_kernel::Result<Vec<_>>>().map_err(to_py)?).map_err(to_py)?,
        };
        let responses = match responses {
            None => vec![Response::Convert, Response::Draw],
            Some(rs) => rs
                .iter()
                .map(|r| {
                    if let Ok(s) = r.extract::<String>() {
                        return match s.as_str() {
                            "convert" => Ok(Response::Convert),
                            "draw" => Ok(Response::Draw),
                            "sell" => Ok(Response::Sell { cost_bp: 0.0 }),
                            s => Err(PyValueError::new_err(format!("a response is convert, draw or (\"sell\", cost_bp), not {s}"))),
                        };
                    }
                    let (what, bp): (String, f64) = r.extract()?;
                    if what == "sell" { Ok(Response::Sell { cost_bp: bp }) } else { Err(PyValueError::new_err(format!("({what}, ...) is not a response"))) }
                })
                .collect::<PyResult<_>>()?,
        };
        let opening = opening.unwrap_or_default();
        let opening_dues = opening_dues
            .unwrap_or_default()
            .iter()
            .map(|d| {
                if let Ok((amount, at, ccy)) = d.extract::<(f64, Bound<'_, PyAny>, String)>() {
                    return Ok((amount, convert::ts(&at)?, Some(ccy)));
                }
                let (amount, at): (f64, Bound<'_, PyAny>) =
                    d.extract().map_err(|_| PyTypeError::new_err("an opening due is (amount, settles_at) or (amount, settles_at, currency)"))?;
                Ok((amount, convert::ts(&at)?, None))
            })
            .collect::<PyResult<Vec<_>>>()?;
        if policies.is_empty() && opening.is_empty() {
            return Err(PyValueError::new_err("a book needs a policy or opening positions"));
        }
        if time_in_force == 0 {
            return Err(PyValueError::new_err("an order works for at least one step"));
        }
        let execution = match execution.filter(|e| !e.is_none()) {
            None => ExecChoice::Fill(Py::new(py, PyFill::research(0.0))?),
            Some(e) => {
                if let Ok(f) = e.cast::<PyFill>() {
                    ExecChoice::Fill(f.clone().unbind())
                } else if let Ok(x) = e.cast::<PyExecution>() {
                    ExecChoice::Process(x.clone().unbind())
                } else {
                    return Err(PyTypeError::new_err("execution is a Fill or an Execution"));
                }
            }
        };
        let holding = match holding {
            "drift" => HoldingPolicy::Drift,
            "reset_weights" => HoldingPolicy::ResetWeights,
            h => return Err(PyValueError::new_err(format!("holding is drift or reset_weights, not {h}"))),
        };
        let cash = match cash {
            "forbid" => CashSpec::Forbid(Breach::Scale),
            "forbid_refuse" => CashSpec::Forbid(Breach::Refuse),
            "forbid_fail" => CashSpec::Forbid(Breach::Fail),
            "borrow" => CashSpec::Borrow { limit: borrow_limit.ok_or_else(|| PyValueError::new_err("borrowing needs a borrow_limit"))?, rate: borrow_rate },
            "unlimited" => CashSpec::Unlimited,
            c => return Err(PyValueError::new_err(format!("cash is forbid, forbid_refuse, forbid_fail, borrow or unlimited, not {c}"))),
        };
        let shorts = match shorts {
            "forbid" => ShortSpec::Forbid,
            "borrow" => ShortSpec::Borrow { fee_bp: borrow_fee_bp, available: borrow_available },
            "unlimited" => ShortSpec::Unlimited,
            s => return Err(PyValueError::new_err(format!("shorts is forbid, borrow or unlimited, not {s}"))),
        };
        let settlement = match settlement.filter(|s| !s.is_none()) {
            None => (0, Vec::new()),
            Some(s) => {
                if let Ok(n) = s.extract::<usize>() {
                    (n, Vec::new())
                } else {
                    let d = s.cast::<PyDict>().map_err(|_| PyTypeError::new_err("settlement is an integer or a dict of steps"))?;
                    let mut default = 0;
                    let mut by = Vec::new();
                    for (k, v) in d.iter() {
                        let k: String = k.extract()?;
                        let v: usize = v.extract()?;
                        if k == "default" {
                            default = v;
                        } else {
                            by.push((k, v));
                        }
                    }
                    (default, by)
                }
            }
        };
        let horizon = match horizon {
            "mark" => Horizon::Mark,
            "liquidate" => Horizon::Liquidate,
            h => return Err(PyValueError::new_err(format!("horizon is mark or liquidate, not {h}"))),
        };
        let flows = match flows.filter(|f| !f.is_none()) {
            None => None,
            Some(f) => {
                let (kind, what, settle): (String, Bound<'_, PyAny>, usize) = f.extract()?;
                match kind.as_str() {
                    "series" => {
                        let spec: String = what.extract()?;
                        let (s, c) = split(&spec)?;
                        Some(FlowSpec::Series { series: s.into(), column: c.into(), settle })
                    }
                    "model" => Some(FlowSpec::Model { callable: what.unbind(), settle }),
                    k => return Err(PyValueError::new_err(format!("flows are fi.flows.series(...) or fi.flows.model(...), not {k}"))),
                }
            }
        };
        let routing = match routing {
            "cash" => Routing::Cash,
            "collections" => Routing::Collections,
            r => return Err(PyValueError::new_err(format!("routing is cash or collections, not {r}"))),
        };
        Ok(Self {
            name,
            policies,
            execution,
            initial_cash,
            holding,
            cash,
            deposit,
            day_count,
            shorts,
            currency,
            fx,
            conversion: FxConversion { auto: auto_conversion, spread_bp: conversion_spread_bp },
            settlement,
            horizon,
            flows,
            tif: time_in_force,
            constraints: constraints.unwrap_or_default(),
            routing,
            journal,
            keep_holdings,
            opening,
            opening_dues,
            financing: financing.unwrap_or_default(),
            responses,
            order,
        })
    }

    /// The book's name.
    #[getter]
    fn name(&self) -> String {
        self.label()
    }

    fn __repr__(&self) -> String {
        format!("Book({}, {} policies, horizon={:?})", self.label(), self.policies.len(), self.horizon)
    }
}

impl PyBook {
    pub(crate) fn label(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.policies.first().map_or_else(|| "book".to_owned(), |p| p.get().name()))
    }

    /// The base currency's name, if the book names one.
    pub(crate) fn currency(&self) -> Option<&str> {
        self.currency.as_deref()
    }

    /// The kernel's book, and where each Python callable keeps its exception.
    pub(crate) fn build(&self, py: Python<'_>, ws: &Workspace, records: &PyRecords) -> PyResult<(Book, Vec<Arc<Errors>>)> {
        let names = &records.names;
        let cal = records.rec.calendar();
        let mut errors = Vec::new();
        let mut policies = Vec::with_capacity(self.policies.len());
        for p in &self.policies {
            let p = p.get();
            let (decide, e) = p.decide.get().build(py, names)?;
            errors.extend(e);
            let scope = p.scope.as_ref().map(|s| s.iter().map(|n| names.id(n)).collect::<PyResult<Vec<EntityId>>>()).transpose()?;
            policies.push(PolicySpec {
                name: p.name(),
                decide,
                schedule: p.schedule.times(cal),
                scope,
                priority: p.priority,
                lag: p.lag,
                time_in_force: p.tif,
            });
        }
        let execution = match &self.execution {
            ExecChoice::Fill(f) => Executor::Fill(f.get().build(names)?),
            ExecChoice::Process(x) => Executor::Process(x.get().build(names)?),
        };
        let base_currency = match &self.currency {
            None => EntityId::NONE,
            Some(c) => ws.entities.get("currency", c).ok_or_else(|| PyValueError::new_err(format!("no currency {c}")))?,
        };
        let cash = match &self.cash {
            CashSpec::Forbid(b) => CashPolicy::Forbid { breach: *b },
            CashSpec::Borrow { limit, rate } => CashPolicy::Borrow { limit: *limit, rate: pair(rate.as_deref())? },
            CashSpec::Unlimited => CashPolicy::Unlimited,
        };
        let shorts = match &self.shorts {
            ShortSpec::Forbid => ShortPolicy::Forbid,
            ShortSpec::Borrow { fee_bp, available } => ShortPolicy::Borrow { fee_bp: *fee_bp, available: pair(available.as_deref())? },
            ShortSpec::Unlimited => ShortPolicy::Unlimited,
        };
        let by_contract = self.settlement.1.iter().map(|(n, l)| Ok((names.id(n)?, *l))).collect::<PyResult<_>>()?;
        let flows = match &self.flows {
            None => None,
            Some(FlowSpec::Series { series, column, settle }) => Some(CapitalFlows::Series { series: series.clone(), column: column.clone(), settle: *settle }),
            Some(FlowSpec::Model { callable, settle }) => {
                let e = Arc::new(Errors::default());
                errors.push(e.clone());
                Some(CapitalFlows::Model { model: Arc::new(PyFlow { callable: callable.clone_ref(py), errors: e }), settle: *settle })
            }
        };
        let constraints = self.constraints.iter().map(|c| c.get().build(ws, names)).collect::<PyResult<_>>()?;
        let config = LedgerConfig {
            initial_cash: self.initial_cash,
            holding: self.holding,
            execution_lag: 0,
            identity_tolerance: 1e-9,
            base_currency,
            fx: pair(self.fx.as_deref())?,
            conversion: self.conversion.clone(),
            cash,
            deposit: self
                .deposit
                .iter()
                .map(|(ccy, rate, dc)| {
                    let currency = match ccy {
                        None => EntityId::NONE,
                        Some(c) => ws.entities.get("currency", c).ok_or_else(|| PyValueError::new_err(format!("a deposit rate names no currency {c}")))?,
                    };
                    let rate = pair(Some(rate))?.ok_or_else(|| PyValueError::new_err("a deposit rate is \"series.column\""))?;
                    Ok(CashRate { currency, rate, day_count: *dc })
                })
                .collect::<PyResult<_>>()?,
            day_count: self.day_count,
            shorts,
            settlement: Settlement { lag: self.settlement.0, by_contract },
            horizon: self.horizon,
            flows,
            time_in_force: self.tif,
            constraints,
            routing: self.routing,
            journal: self.journal,
            keep_holdings: self.keep_holdings,
            opening: self.opening.iter().map(|(n, q)| Ok((names.id(n)?, *q))).collect::<PyResult<_>>()?,
            opening_dues: self
                .opening_dues
                .iter()
                .map(|(amount, at, ccy)| {
                    let currency = match ccy {
                        None => EntityId::NONE,
                        Some(c) => ws.entities.get("currency", c).ok_or_else(|| PyValueError::new_err(format!("an opening due names no currency {c}")))?,
                    };
                    Ok(OpeningDue { currency, amount: *amount, settles_at: *at })
                })
                .collect::<PyResult<_>>()?,
            distributor: None,
            financing: self.financing.iter().map(|a| a.get().build(names)).collect::<PyResult<_>>()?,
            responses: self.responses.clone(),
            order: self.order.clone(),
        };
        Ok((Book { name: self.label(), policies, execution, config }, errors))
    }
}

/// A Nautilus `FuturesContract` from its fields, checked by Nautilus's own builder.
#[allow(clippy::too_many_arguments)]
pub(crate) fn futures_contract(
    symbol: &str,
    venue: &str,
    underlying: &str,
    activation: Option<&Bound<'_, PyAny>>,
    expiration: &Bound<'_, PyAny>,
    currency: &str,
    price_increment: &str,
    multiplier: f64,
    lot_size: f64,
    asset_class: &str,
) -> PyResult<nautilus_model::instruments::FuturesContract> {
    use nautilus_core::UnixNanos;
    use nautilus_model::enums::AssetClass;
    use nautilus_model::identifiers::{InstrumentId, Symbol, Venue};
    use nautilus_model::types::{Currency, Price, Quantity};
    let nanos = |t: &Bound<'_, PyAny>| -> PyResult<UnixNanos> {
        Ok(UnixNanos::from(u64::try_from(crate::convert::ts(t)?.0).map_err(|_| PyValueError::new_err("a time before 1970"))?))
    };
    let class = match asset_class {
        "debt" => AssetClass::Debt,
        "index" => AssetClass::Index,
        "fx" => AssetClass::FX,
        "commodity" => AssetClass::Commodity,
        "equity" => AssetClass::Equity,
        a => return Err(PyValueError::new_err(format!("no asset class {a:?} (debt, index, fx, commodity, equity)"))),
    };
    let tick =
        <Price as std::str::FromStr>::from_str(price_increment).map_err(|e| PyValueError::new_err(format!("price_increment {price_increment:?}: {e}")))?;
    let ccy = Currency::try_from_str(currency).ok_or_else(|| PyValueError::new_err(format!("Nautilus's currency registry has no {currency}")))?;
    let qty = |x: f64, what: &str| Quantity::new_checked(x, 0).map_err(|e| PyValueError::new_err(format!("{what} {x}: {e}")));
    let activation = activation.map(nanos).transpose()?.unwrap_or_default();
    nautilus_model::instruments::FuturesContract::builder()
        .instrument_id(InstrumentId::new(Symbol::from(symbol), Venue::from(venue)))
        .raw_symbol(Symbol::from(symbol))
        .asset_class(class)
        .exchange(ustr::Ustr::from(venue))
        .underlying(ustr::Ustr::from(underlying))
        .activation_ns(activation)
        .expiration_ns(nanos(expiration)?)
        .currency(ccy)
        .price_precision(tick.precision)
        .price_increment(tick)
        .multiplier(qty(multiplier, "multiplier")?)
        .lot_size(qty(lot_size, "lot_size")?)
        .ts_event(activation)
        .ts_init(activation)
        .build()
        .map_err(|e| PyValueError::new_err(format!("Nautilus refuses the future {symbol}: {e}")))
}
