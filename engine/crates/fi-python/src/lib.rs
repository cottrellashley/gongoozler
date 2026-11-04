//! Python bindings: the `fi_engine._native` module.
//!
//! The engine from Python, as the same objects the Rust crates have:
//!
//! * a [`Workspace`](PyWorkspace) of series, each fed by any Arrow frame (Polars, pandas, pyarrow,
//!   DuckDB) and checked against its declaration; the engine never reads data itself;
//! * contracts, unit records and composites, and debt from its terms ([`terms`]): business
//!   calendars, coupons, bonds, curves and their analytics;
//! * models: built in, or Python callables ([`bridge::PyCrossSection`]);
//! * decisions: built in, or Python callables ([`bridge::PyDecision`]);
//! * fills: research, request for quote, or Nautilus order books;
//! * research runs and parallel sweeps, with results as Polars frames.
//!
//! Every long computation runs with the GIL released, and under `gongoozler run` reports its
//! progress into the watcher's ring ([`progress`]). The `fi_engine` Python package adds a few
//! helpers on top (`python/fi_engine`).

mod book;
mod bridge;
mod convert;
mod deal;
mod progress;
mod terms;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use pyo3::create_exception;
use pyo3::exceptions::{PyException, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use pyo3_arrow::PyTable;

use fi_exec::{BookFill, Level, RfqFill};
use fi_io::{Known, SeriesDecl, ValueSpec};
use fi_kernel::{
    Calendar, Composite, Contract, DecisionModel, Entities, EntityId, Error, FillModel, Graph, InCurrency, Information, Input, KnowledgeBasis, KnowledgePolicy,
    Layout, LedgerOutput, MissingPolicy, ModelRole, ModelSpec, Outcomes, Producer, ResearchFill, Role, Shape, Transition, Ts, Unit, UnitRecords, Vintaged,
    Workspace, hash::Hash64,
};
use fi_research::Book;
use fi_std::{
    Branch, CashAccount, DailyFuture, Defaults, EwmaVol, GaussianPaths, LoanTerms, Merton, NelsonSiegel, Reinvest, Replay, RiskParity, RiskParityMethod,
    SuppliedBond, TopN, TotalReturn, Trades, ZScore, synth,
};

use book::{PyAgreement, PyBook, PyConstraint, PyExecution, PyPolicy};
use bridge::{Errors, Names, PyCrossSection, PyDecision};

create_exception!(fi_engine, EngineError, PyException, "An error from the engine.");
create_exception!(fi_engine, InvalidError, EngineError, "Input that breaks its contract: types, duplicates, NaN, a broken identity.");
create_exception!(fi_engine, UnknownError, EngineError, "A named series, column, entity or contract that does not exist.");
create_exception!(fi_engine, MissingError, EngineError, "A value the run needs is missing, and the policy says fail.");

/// A kernel error as a Python exception.
pub(crate) fn to_py(e: Error) -> PyErr {
    match e {
        Error::Invalid(m) => InvalidError::new_err(m),
        Error::Unknown(m) => UnknownError::new_err(m),
        Error::Missing(m) => MissingError::new_err(m),
    }
}

/// A contract in a currency, if one is named.
fn in_currency(c: Arc<dyn Contract>, ccy: Option<EntityId>) -> Arc<dyn Contract> {
    match ccy {
        Some(currency) => Arc::new(InCurrency { contract: c, currency }),
        None => c,
    }
}

/// The first Python exception any bridge kept, else the kernel's error.
fn raised(e: Error, errors: &[&Errors]) -> PyErr {
    errors.iter().find_map(|x| x.take()).unwrap_or_else(|| to_py(e))
}

fn days(d: f64) -> i64 {
    (d * Ts::DAY as f64).round() as i64
}

/// A transition from Python: `(at, from, {into: units}, cash, role, note)` (`fi.transition`).
fn transition(obj: &Bound<'_, PyAny>, names: &Names) -> PyResult<Transition> {
    let (at, from, into, cash, role, note): (Bound<'_, PyAny>, String, HashMap<String, f64>, f64, String, String) =
        obj.extract().map_err(|_| PyTypeError::new_err("a transition is fi.transition(at, from_, into, cash=..., role=..., note=...)"))?;
    let role = match role.as_str() {
        "principal" => Role::Principal,
        "recovery" => Role::Recovery,
        "exchange" => Role::Exchange,
        "income" => Role::Income,
        "price" => Role::Performance,
        "fee" => Role::Fee,
        r => return Err(PyValueError::new_err(format!("a transition books to principal, recovery, exchange, income, price or fee, not {r}"))),
    };
    let mut into: Vec<(EntityId, f64)> = into.iter().map(|(n, r)| Ok((names.id(n)?, *r))).collect::<PyResult<_>>()?;
    into.sort_by_key(|(e, _)| e.0);
    Ok(Transition { at: convert::ts(&at)?, from: names.id(&from)?, into, cash, role, note })
}

fn split(spec: &str) -> PyResult<(&str, &str)> {
    spec.split_once('.').ok_or_else(|| PyValueError::new_err(format!("expected series.column, not {spec:?}")))
}

// ------------------------------------------------------------------------------------- calendars

/// The economic clock: sorted, distinct times.
#[pyclass(name = "Calendar", module = "fi_engine._native", frozen)]
#[derive(Clone)]
pub struct PyCalendar {
    cal: Calendar,
}

#[pymethods]
impl PyCalendar {
    /// A calendar of the given times (nanoseconds, dates, datetimes or ISO dates).
    #[new]
    fn new(times: Vec<Bound<'_, PyAny>>) -> PyResult<Self> {
        Ok(Self { cal: Calendar::new(times.iter().map(convert::ts).collect::<PyResult<_>>()?) })
    }

    /// Every weekday from `start` to `end`, inclusive.
    #[staticmethod]
    fn weekdays(start: &Bound<'_, PyAny>, end: &Bound<'_, PyAny>) -> PyResult<Self> {
        Ok(Self { cal: Calendar::weekdays(convert::ts(start)?, convert::ts(end)?) })
    }

    fn __len__(&self) -> usize {
        self.cal.len()
    }

    /// The times, in nanoseconds.
    fn times(&self) -> Vec<i64> {
        self.cal.times().iter().map(|t| t.0).collect()
    }

    /// The times as dates.
    fn dates<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyList>> {
        PyList::new(py, self.cal.times().iter().map(|t| convert::date(py, *t)).collect::<PyResult<Vec<_>>>()?)
    }

    /// The last time of each month, in nanoseconds.
    fn month_ends(&self) -> Vec<i64> {
        self.cal.month_ends().iter().map(|t| t.0).collect()
    }

    /// Every `step`-th time from `offset`, in nanoseconds.
    #[pyo3(signature = (step, offset = 0))]
    fn every(&self, step: usize, offset: usize) -> Vec<i64> {
        self.cal.every(step, offset).iter().map(|t| t.0).collect()
    }

    /// The first `n` times.
    fn prefix(&self, n: usize) -> Self {
        Self { cal: self.cal.prefix(n) }
    }

    /// Times in either calendar.
    fn union(&self, other: &Self) -> Self {
        Self { cal: Calendar::union(&[&self.cal, &other.cal]) }
    }

    /// Times in both calendars.
    fn intersection(&self, other: &Self) -> Self {
        Self { cal: Calendar::intersection(&[&self.cal, &other.cal]) }
    }

    /// Content hash.
    #[getter]
    fn version(&self) -> u64 {
        self.cal.version()
    }

    fn __repr__(&self) -> String {
        match (self.cal.times().first(), self.cal.times().last()) {
            (Some(a), Some(b)) => format!("Calendar({} times, {a} to {b})", self.cal.len()),
            _ => "Calendar(empty)".into(),
        }
    }
}

// ------------------------------------------------------------------------------------------ models

/// A model input: `series.column`, read exactly at each time, as of it, market-wide, or through
/// a relationship.
#[pyclass(name = "Input", module = "fi_engine._native", frozen)]
#[derive(Clone)]
pub struct PyInput {
    input: Input,
}

#[pymethods]
impl PyInput {
    /// The value at exactly the evaluation time.
    #[staticmethod]
    fn exact(spec: &str) -> PyResult<Self> {
        let (s, c) = split(spec)?;
        Ok(Self { input: Input::exact(s, c) })
    }

    /// The latest value at or before the evaluation time (static attributes, filings).
    #[staticmethod]
    fn asof(spec: &str) -> PyResult<Self> {
        let (s, c) = split(spec)?;
        Ok(Self { input: Input::asof(s, c) })
    }

    /// A market-wide value (a series with no entity), as of the evaluation time.
    #[staticmethod]
    fn market(spec: &str) -> PyResult<Self> {
        let (s, c) = split(spec)?;
        Ok(Self { input: Input::market(s, c) })
    }

    /// The same input, reached through relationship `series.column` (a bond's issuer).
    fn via(&self, spec: &str) -> PyResult<Self> {
        let (s, c) = split(spec)?;
        Ok(Self { input: self.input.clone().via(s, c) })
    }

    fn __repr__(&self) -> String {
        let i = &self.input;
        let how = if i.market {
            "market"
        } else if i.asof {
            "asof"
        } else {
            "exact"
        };
        let via = i.via.as_ref().map_or(String::new(), |(s, c)| format!(".via({s}.{c})"));
        format!("Input.{how}({}.{}){via}", i.series, i.column)
    }
}

/// An input from an `Input`, or from `"series.column"` read exactly (or as of, if `asof`).
fn input(obj: &Bound<'_, PyAny>, asof: bool) -> PyResult<Input> {
    if let Ok(i) = obj.cast::<PyInput>() {
        return Ok(i.get().input.clone());
    }
    let spec: String = obj.extract().map_err(|_| PyTypeError::new_err("an input is an Input or a \"series.column\" string"))?;
    let (s, c) = split(&spec)?;
    Ok(if asof { Input::asof(s, c) } else { Input::exact(s, c) })
}

/// A model: built in, or a Python callable. Evaluating it writes a series named after it.
#[pyclass(name = "Model", module = "fi_engine._native", frozen)]
pub struct PyModel {
    model: fi_kernel::Model,
    python: Option<Arc<PyCrossSection>>,
    /// Where the Python callables inside it (a deal manager's decisions) keep their exceptions.
    errors: Vec<Arc<Errors>>,
}

#[pymethods]
impl PyModel {
    /// Z-score of `input` across the universe at each time, within groups if `group` is given
    /// (a category, read as of the time).
    #[staticmethod]
    #[pyo3(signature = (name, input, *, group = None, ddof = 0.0, min_count = 2))]
    fn zscore(name: &str, input: &Bound<'_, PyAny>, group: Option<&Bound<'_, PyAny>>, ddof: f64, min_count: usize) -> PyResult<Self> {
        let m = ZScore { name: name.into(), input: self::input(input, false)?, group: group.map(|g| self::input(g, true)).transpose()?, ddof, min_count };
        Ok(Self { model: fi_kernel::Model::cross_section(m), python: None, errors: Vec::new() })
    }

    /// A Nelson–Siegel curve fitted each date to `yields` against `maturity` (a date column, in
    /// days since the epoch): outputs `fitted` and `residual`.
    #[staticmethod]
    #[pyo3(signature = (name, yields, maturity, *, lam = 1.8, min_points = 5))]
    fn nelson_siegel(name: &str, yields: &Bound<'_, PyAny>, maturity: &Bound<'_, PyAny>, lam: f64, min_points: usize) -> PyResult<Self> {
        let m = NelsonSiegel { name: name.into(), yields: input(yields, false)?, maturity: input(maturity, true)?, lambda: lam, min_points };
        Ok(Self { model: fi_kernel::Model::cross_section(m), python: None, errors: Vec::new() })
    }

    /// Exponentially weighted volatility of log changes in a level: output `vol`.
    #[staticmethod]
    #[pyo3(signature = (name, input, *, halflife = 20.0, annualise = 252.0, warmup = 5))]
    fn ewma_vol(name: &str, input: &Bound<'_, PyAny>, halflife: f64, annualise: f64, warmup: usize) -> PyResult<Self> {
        let m = EwmaVol { name: name.into(), input: self::input(input, false)?, halflife, annualise, warmup };
        Ok(Self { model: fi_kernel::Model::time_series(m), python: None, errors: Vec::new() })
    }

    /// Merton's model per entity: equity (exact), debt (as of: filings) and a rate (market-wide):
    /// outputs `dd`, `pd` and `spread`.
    #[staticmethod]
    #[pyo3(signature = (name, equity, debt, rate, *, horizon = 1.0, window = 252, annualise = 252.0))]
    fn merton(
        name: &str,
        equity: &Bound<'_, PyAny>,
        debt: &Bound<'_, PyAny>,
        rate: &Bound<'_, PyAny>,
        horizon: f64,
        window: usize,
        annualise: f64,
    ) -> PyResult<Self> {
        let rate = match rate.extract::<String>() {
            Ok(spec) => {
                let (s, c) = split(&spec)?;
                Input::market(s, c)
            }
            Err(_) => input(rate, false)?,
        };
        let m = Merton { name: name.into(), equity: input(equity, false)?, debt: input(debt, true)?, rate, horizon, window, annualise };
        Ok(Self { model: fi_kernel::Model::time_series(m), python: None, errors: Vec::new() })
    }

    /// A Python cross-sectional model: `compute(t, entities, x)` is called once per time, in time
    /// order, with `x` one NumPy array per input over the universe; it returns one array per
    /// output (or a dict by output name). Each Python model is its own cache entry unless a
    /// `version` says two are the same.
    #[staticmethod]
    #[pyo3(signature = (name, compute, inputs, outputs, *, universe = None, window_days = 1.0, lag_days = 0.0, version = None))]
    #[allow(clippy::too_many_arguments)]
    fn python(
        name: &str,
        compute: Py<PyAny>,
        inputs: Vec<Bound<'_, PyAny>>,
        outputs: Vec<String>,
        universe: Option<String>,
        window_days: f64,
        lag_days: f64,
        version: Option<u64>,
    ) -> PyResult<Self> {
        let params = version.map_or_else(bridge::fresh, |v| {
            let mut h = Hash64::new();
            h.str("python");
            h.u64(v);
            h.finish()
        });
        let mut spec = ModelSpec::new(name, Shape::CrossSection).serial().window(days(window_days)).lag(days(lag_days)).params(params);
        for i in &inputs {
            spec = spec.input(input(i, false)?);
        }
        for o in &outputs {
            spec = spec.output(o);
        }
        if let Some(u) = &universe {
            spec = spec.universe(u);
        }
        let p = Arc::new(PyCrossSection::new(spec, compute));
        Ok(Self { model: fi_kernel::Model::CrossSection(p.clone()), python: Some(p), errors: Vec::new() })
    }

    /// The model's name (its output series).
    #[getter]
    fn name(&self) -> String {
        self.model.spec().name
    }

    /// The same model with a role: `"signal"` (the default), `"real_world"` (how the world
    /// evolves), `"pricing"` (values from a state) or `"stress"` (a named scenario override).
    fn role(&self, role: &str) -> PyResult<Self> {
        let r = parse_role(role)?;
        Ok(self.amended(move |s| s.role(r)))
    }

    /// The same model, available only from `t`: a point-in-time run has no output before then.
    fn available_from(&self, t: &Bound<'_, PyAny>) -> PyResult<Self> {
        let t = convert::ts(t)?;
        Ok(self.amended(move |s| s.available_from(t)))
    }

    /// The same model used before it was available: a labelled retrospective experiment.
    fn retrospective(&self) -> Self {
        self.amended(ModelSpec::retrospective)
    }

    /// The same model, declaring the state its output describes.
    fn describes(&self, state: String) -> Self {
        self.amended(move |s| s.describes(&state))
    }

    /// Versions of one cross-sectional model, each used from when it became available:
    /// `versions` is a list of `(time, model)`. A retrospective run uses the latest throughout.
    #[staticmethod]
    #[pyo3(signature = (name, versions, *, retrospective = false))]
    fn vintaged(name: &str, versions: Vec<(Bound<'_, PyAny>, PyRef<'_, PyModel>)>, retrospective: bool) -> PyResult<Self> {
        let v = versions.iter().map(|(t, m)| Ok((convert::ts(t)?, m.model.clone()))).collect::<PyResult<Vec<_>>>()?;
        let m = Vintaged::new(name, v, retrospective).map_err(to_py)?;
        Ok(Self { model: fi_kernel::Model::cross_section(m), python: None, errors: Vec::new() })
    }

    /// The series it writes.
    #[getter]
    fn writes(&self) -> Vec<String> {
        self.model.spec().produced()
    }

    /// What the model's definition models (a deal's waterfall features, a generator's).
    #[getter]
    fn supports(&self) -> Vec<&'static str> {
        match &self.model {
            fi_kernel::Model::Batch(b) => b.supports(),
            _ => Vec::new(),
        }
    }

    /// Output columns.
    #[getter]
    fn outputs(&self) -> Vec<String> {
        self.model.spec().outputs
    }

    fn __repr__(&self) -> String {
        let s = self.model.spec();
        let kind = if self.python.is_some() { "python" } else { "built-in" };
        format!("Model({}, {kind}, {:?}, outputs={:?})", s.name, s.shape, s.outputs)
    }
}

impl PyModel {
    fn amended(&self, f: impl Fn(ModelSpec) -> ModelSpec + Send + Sync + 'static) -> Self {
        Self { model: self.model.clone().amend(f), python: self.python.clone(), errors: self.errors.clone() }
    }
}

fn parse_role(role: &str) -> PyResult<ModelRole> {
    match role {
        "signal" => Ok(ModelRole::Signal),
        "real_world" => Ok(ModelRole::RealWorld),
        "pricing" => Ok(ModelRole::Pricing),
        "stress" => Ok(ModelRole::Stress),
        r => Err(PyValueError::new_err(format!("a role is signal, real_world, pricing or stress, not {r}"))),
    }
}

fn role_name(role: ModelRole) -> &'static str {
    match role {
        ModelRole::Signal => "signal",
        ModelRole::RealWorld => "real_world",
        ModelRole::Pricing => "pricing",
        ModelRole::Stress => "stress",
    }
}

// --------------------------------------------------------------------------------------- contracts

/// A set of contracts (instruments), to step into unit records.
#[pyclass(name = "Contracts", module = "fi_engine._native", frozen)]
#[derive(Clone)]
pub struct PyContracts {
    inner: Vec<Arc<dyn Contract>>,
}

/// A run's manifest (spec, change 7): equal manifests give equal outputs, and `diff` attributes
/// any difference to the series, models and settings that changed.
#[pyclass(name = "Manifest", module = "fi_engine._native", frozen)]
pub struct PyManifest {
    m: fi_research::Manifest,
}

#[pymethods]
impl PyManifest {
    /// A hash of everything that determines the output.
    #[getter]
    fn fingerprint(&self) -> u64 {
        self.m.fingerprint()
    }

    /// The run's mode.
    #[getter]
    fn mode(&self) -> String {
        self.m.mode.clone()
    }

    /// What differs from `other`, attributed: one line per series, model, setting or output.
    fn diff(&self, other: &Self) -> Vec<String> {
        self.m.diff(&other.m)
    }

    /// The manifest as JSON.
    fn json(&self) -> String {
        self.m.to_json()
    }

    /// The manifest as a dict.
    fn as_dict<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        py.import("json")?.call_method1("loads", (self.m.to_json(),))
    }

    fn __eq__(&self, other: &Self) -> bool {
        self.m == other.m
    }

    fn __repr__(&self) -> String {
        format!(
            "Manifest({}, {} series, {} models, mode={}, fingerprint={:016x})",
            self.m.book.0,
            self.m.series.len(),
            self.m.models.len(),
            self.m.mode,
            self.m.fingerprint()
        )
    }
}

#[pymethods]
impl PyContracts {
    /// What these contracts' definitions model, generated from the definitions: a Polars frame of
    /// kind and feature.
    fn support_matrix<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let m = fi_research::support_matrix(&self.inner, &[]);
        let d = PyDict::new(py);
        d.set_item("kind", m.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>())?;
        d.set_item("feature", m.iter().map(|(_, f)| f.clone()).collect::<Vec<_>>())?;
        py.import("polars")?.call_method1("DataFrame", (d,))
    }

    fn __len__(&self) -> usize {
        self.inner.len()
    }

    fn __add__(&self, other: &Self) -> Self {
        Self { inner: self.inner.iter().chain(&other.inner).cloned().collect() }
    }

    fn __repr__(&self) -> String {
        let mut kinds: Vec<&str> = self.inner.iter().map(|c| c.kind()).collect();
        kinds.sort_unstable();
        kinds.dedup();
        format!("Contracts({}, kinds={kinds:?})", self.inner.len())
    }
}

/// Unit records: every contract's value, cash and P&L terms per unit at every calendar time.
#[pyclass(name = "Records", module = "fi_engine._native", frozen)]
pub struct PyRecords {
    rec: Arc<UnitRecords>,
    names: Arc<Names>,
    diagnostics: (u64, u64, u64, u64),
    /// The series the contracts read: the world a run on these records is in.
    reads: Arc<Vec<String>>,
    /// Approximations declared when they were prepared.
    approximations: Arc<Vec<String>>,
}

#[pymethods]
impl PyRecords {
    /// The approximations declared when they were prepared.
    #[getter]
    fn approximations(&self) -> Vec<String> {
        self.approximations.to_vec()
    }

    fn __len__(&self) -> usize {
        self.rec.entities().len()
    }

    /// Contract names, in row order.
    #[getter]
    fn contracts(&self) -> Vec<String> {
        self.names.names.clone()
    }

    /// The calendar.
    #[getter]
    fn calendar(&self) -> PyCalendar {
        PyCalendar { cal: self.rec.calendar().clone() }
    }

    /// What the contracts did not observe: values carried stale, defaulted, missing.
    #[getter]
    fn diagnostics<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        d.set_item("stale", self.diagnostics.0)?;
        d.set_item("defaults", self.diagnostics.1)?;
        d.set_item("missing", self.diagnostics.2)?;
        Ok(d)
    }

    /// Content hash.
    #[getter]
    fn version(&self) -> u64 {
        self.rec.version()
    }

    /// The records as a long Polars frame: contract, time, value, cash, terms, known, state.
    fn frame<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let names: Vec<&str> = self.names.names.iter().map(String::as_str).collect();
        convert::to_python(py, fi_io::records_frame_named(&self.rec, &names).map_err(convert::err)?)
    }

    fn __repr__(&self) -> String {
        format!("Records({} contracts × {} times)", self.rec.entities().len(), self.rec.calendar().len())
    }
}

// --------------------------------------------------------------------------------------- decisions

/// A decision model, and where the Python callables in it keep their exceptions.
pub(crate) type BuiltDecision = (Arc<dyn DecisionModel>, Vec<Arc<Errors>>);

enum DecisionSpec {
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
    spec: DecisionSpec,
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
    fn name(&self) -> String {
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

    fn build(&self, py: Python<'_>, names: &Arc<Names>) -> PyResult<BuiltDecision> {
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

// ------------------------------------------------------------------------------------------- fills

/// One contract's order book, as given.
struct BookSpec {
    contract: String,
    bids: Vec<Level>,
    asks: Vec<Level>,
    mark: f64,
    per_unit: f64,
}

enum FillSpec {
    Research(f64),
    Rfq(RfqFill),
    Book { books: Vec<BookSpec>, price_precision: u8, size_precision: u8 },
}

/// How orders fill: at research fidelity, by request for quote, or against Nautilus order books.
#[pyclass(name = "Fill", module = "fi_engine._native", frozen)]
pub struct PyFill {
    spec: FillSpec,
}

fn levels(obj: Option<Bound<'_, PyAny>>) -> PyResult<Vec<Level>> {
    let Some(obj) = obj else { return Ok(Vec::new()) };
    let pairs: Vec<(f64, f64)> = obj.extract()?;
    Ok(pairs.into_iter().map(|(price, size)| Level { price, size }).collect())
}

#[pymethods]
impl PyFill {
    /// Every order fills in full at the unit's value plus a half spread (in basis points).
    #[staticmethod]
    #[pyo3(signature = (half_spread_bp = 0.0))]
    fn research(half_spread_bp: f64) -> Self {
        Self { spec: FillSpec::Research(half_spread_bp) }
    }

    /// Request for quote across a dealer panel: dealers respond with probability `response`,
    /// quote around the value, and the best quote fills up to `max_size`. Deterministic per seed.
    #[staticmethod]
    #[pyo3(signature = (*, dealers = 5, response = 0.8, half_spread_bp = 5.0, dispersion_bp = 3.0, max_size = f64::INFINITY, seed = 0))]
    fn rfq(dealers: usize, response: f64, half_spread_bp: f64, dispersion_bp: f64, max_size: f64, seed: u64) -> Self {
        Self { spec: FillSpec::Rfq(RfqFill { dealers, response, half_spread_bp, dispersion_bp, max_size, seed }) }
    }

    /// Fills against Nautilus L2 order books: `books` maps a contract to
    /// `{"bids": [(price, size)], "asks": [...], "mark": m, "per_unit": p}`. A fill's distance
    /// from the mark, times `per_unit`, is its cost per unit.
    #[staticmethod]
    #[pyo3(signature = (books, *, price_precision = 4, size_precision = 0))]
    fn book(books: &Bound<'_, PyDict>, price_precision: u8, size_precision: u8) -> PyResult<Self> {
        let mut out = Vec::new();
        for (k, v) in books.iter() {
            let v = v.cast::<PyDict>().map_err(|_| PyTypeError::new_err("each book is a dict with bids, asks, mark and per_unit"))?;
            let mark: f64 = v.get_item("mark")?.ok_or_else(|| PyValueError::new_err("a book needs a mark"))?.extract()?;
            let per_unit: f64 = v.get_item("per_unit")?.map_or(Ok(1.0), |p| p.extract())?;
            out.push(BookSpec { contract: k.extract()?, bids: levels(v.get_item("bids")?)?, asks: levels(v.get_item("asks")?)?, mark, per_unit });
        }
        Ok(Self { spec: FillSpec::Book { books: out, price_precision, size_precision } })
    }

    fn __repr__(&self) -> String {
        match &self.spec {
            FillSpec::Research(bp) => format!("Fill.research({bp} bp)"),
            FillSpec::Rfq(r) => format!("Fill.{}", r.name()),
            FillSpec::Book { books, .. } => format!("Fill.book({} books)", books.len()),
        }
    }
}

impl PyFill {
    fn build(&self, names: &Names) -> PyResult<Arc<dyn FillModel>> {
        Ok(match &self.spec {
            FillSpec::Research(bp) => Arc::new(ResearchFill { half_spread_bp: *bp }),
            FillSpec::Rfq(r) => Arc::new(r.clone()),
            FillSpec::Book { books, price_precision, size_precision } => {
                let mut f = BookFill::new(*price_precision, *size_precision);
                for b in books {
                    f.set_book(names.id(&b.contract)?, &b.bids, &b.asks, b.mark, b.per_unit);
                }
                Arc::new(f)
            }
        })
    }
}

// --------------------------------------------------------------------------------------------- runs

/// A run's accounts: NAV, cash, P&L by term, costs, turnover, holdings.
#[pyclass(name = "Run", module = "fi_engine._native", frozen)]
pub struct PyRun {
    name: String,
    out: LedgerOutput,
    names: Arc<Names>,
    entities: Arc<Entities>,
    mode: String,
}

fn finish(
    out: fi_kernel::Result<LedgerOutput>,
    errors: &[Arc<Errors>],
    records: &PyRecords,
    entities: &Arc<Entities>,
    book: &Book,
    ws: &Workspace,
) -> PyResult<PyRun> {
    let mode = fi_research::run_mode(ws, &records.reads, book).to_string();
    match out {
        Ok(out) => Ok(PyRun { name: book.name.clone(), out, names: records.names.clone(), entities: entities.clone(), mode }),
        Err(e) => Err(raised(e, &errors.iter().map(AsRef::as_ref).collect::<Vec<_>>())),
    }
}

#[pymethods]
impl PyRun {
    /// What the run is, derived from its decisions and its world (spec, change 4): `"holdings
    /// replay"`, `"policy backtest"`, `"scenario"`, `"counterfactual"` or `"simulation"`.
    #[getter]
    fn mode(&self) -> String {
        self.mode.clone()
    }

    /// The strategy's name.
    #[getter]
    fn name(&self) -> String {
        self.name.clone()
    }

    /// The accounts as a Polars frame, one row per calendar time: NAV, cash, the P&L terms
    /// (income, price, principal, other, financing, currency), costs, capital flows, unsettled
    /// amounts, NAV per unit, turnover, holdings.
    fn ledger<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        convert::to_python(py, fi_io::ledger_frame(&self.out).map_err(convert::err)?)
    }

    /// Every transfer with its cause, as a Polars frame (the book must keep a journal).
    fn journal<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        convert::to_python(py, fi_io::journal_frame(&self.out, &self.entities).map_err(convert::err)?)
    }

    /// Each financing agreement at every time, as a Polars frame: agreement, time, balance,
    /// capacity (a CSA: the netting set's value), call and value (a CSA: collateral required).
    fn financing<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        convert::to_python(py, fi_io::financing_frame(&self.out).map_err(convert::err)?)
    }

    /// Every order's life, as a Polars frame (the book must keep a journal).
    fn orders<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        convert::to_python(py, fi_io::orders_frame(&self.out, &self.entities).map_err(convert::err)?)
    }

    /// The holdings at every time, as a Polars frame (the book must keep them).
    fn holdings<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        convert::to_python(py, fi_io::holdings_frame(&self.out, &self.entities).map_err(convert::err)?)
    }

    /// The orders' totals: submitted, filled, partial, expired, cancelled, requested and filled
    /// value, the delay cost from decision to fill, and what the unfilled part missed.
    #[getter]
    fn order_stats<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let o = &self.out.orders;
        let d = PyDict::new(py);
        d.set_item("submitted", o.submitted)?;
        d.set_item("filled", o.filled)?;
        d.set_item("partial", o.partial)?;
        d.set_item("expired", o.expired)?;
        d.set_item("cancelled", o.cancelled)?;
        d.set_item("requested_value", o.requested_value)?;
        d.set_item("filled_value", o.filled_value)?;
        d.set_item("fill_rate", if o.requested_value > 0.0 { o.filled_value / o.requested_value } else { f64::NAN })?;
        d.set_item("delay_cost", o.delay_cost)?;
        d.set_item("missed", o.missed)?;
        Ok(d)
    }

    /// Each constraint's breaches: count, first time, worst excess, penalty.
    #[getter]
    fn breaches<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyList>> {
        let out = PyList::empty(py);
        for b in &self.out.breaches {
            let d = PyDict::new(py);
            d.set_item("constraint", &b.constraint)?;
            d.set_item("hard", b.hard)?;
            d.set_item("count", b.count)?;
            d.set_item("first", b.first.map(|t| convert::date(py, t)).transpose()?)?;
            d.set_item("worst", b.worst)?;
            d.set_item("penalty", b.penalty)?;
            out.append(d)?;
        }
        Ok(out)
    }

    /// When the book failed economically (an obligation it could not pay, or NAV at or below
    /// zero), or `None`. A failed run stays a result.
    #[getter]
    fn failed_at<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyAny>>> {
        self.out.failed_at.map(|t| convert::date(py, t)).transpose()
    }

    /// The largest obligation the cash could not meet.
    #[getter]
    fn shortfall(&self) -> f64 {
        self.out.shortfall
    }

    /// Purchases cut or refused by the cash guard.
    #[getter]
    fn cash_limited(&self) -> u64 {
        self.out.cash_limited
    }

    /// Sales cut by the short guard.
    #[getter]
    fn short_limited(&self) -> u64 {
        self.out.short_limited
    }

    /// NAV after the horizon liquidation, where the book liquidates.
    #[getter]
    fn liquidation_nav(&self) -> Option<f64> {
        self.out.liquidation_nav
    }

    /// Marked value of what the horizon liquidation could not sell.
    #[getter]
    fn liquidation_residual(&self) -> f64 {
        self.out.liquidation_residual
    }

    /// Cash at the end, by currency.
    #[getter]
    fn final_cash<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        for (c, v) in &self.out.final_cash {
            d.set_item(if *c == EntityId::NONE { "base" } else { self.entities.name(*c) }, v)?;
        }
        Ok(d)
    }

    /// Total return.
    #[getter]
    fn total_return(&self) -> f64 {
        self.out.total_return()
    }

    /// NAV at the end.
    #[getter]
    fn final_nav(&self) -> f64 {
        self.out.nav.last().copied().unwrap_or(f64::NAN)
    }

    /// Trades made.
    #[getter]
    fn trades(&self) -> usize {
        self.out.trades
    }

    /// Trades not made: the unit had no value or no measure at the time.
    #[getter]
    fn skipped_trades(&self) -> u64 {
        self.out.skipped_trades
    }

    /// Orders that filled less than they asked, including not at all (no quote, a size cap, a
    /// book without depth).
    #[getter]
    fn unfilled_orders(&self) -> u64 {
        self.out.unfilled_orders
    }

    /// Decisions still waiting to execute when the calendar ended.
    #[getter]
    fn pending_at_end(&self) -> u64 {
        self.out.pending_at_end
    }

    /// The largest accounting-identity error, as a fraction of NAV.
    #[getter]
    fn max_identity_error(&self) -> f64 {
        self.out.max_identity_error
    }

    /// Holdings at the end: contract name to quantity.
    #[getter]
    fn final_holdings<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let by_id: HashMap<EntityId, &str> = self.names.ids.iter().zip(&self.names.names).map(|(e, n)| (*e, n.as_str())).collect();
        let d = PyDict::new(py);
        for (e, q) in &self.out.final_holdings {
            d.set_item(by_id.get(e).copied().unwrap_or("?"), q)?;
        }
        Ok(d)
    }

    /// Content hash of the accounts.
    #[getter]
    fn version(&self) -> u64 {
        self.out.version()
    }

    /// Statistics from Nautilus's analysis crate, against `benchmark` if given; `period` is
    /// observations per year.
    #[pyo3(signature = (benchmark = None, period = 252))]
    fn report<'py>(&self, py: Python<'py>, benchmark: Option<&Self>, period: usize) -> PyResult<Bound<'py, PyDict>> {
        let r = fi_metrics::report(&self.out, benchmark.map(|b| &b.out), period);
        let d = PyDict::new(py);
        for (k, v) in [
            ("total_return", Some(r.total_return)),
            ("sharpe", Some(r.sharpe)),
            ("sortino", Some(r.sortino)),
            ("volatility", Some(r.volatility)),
            ("max_drawdown", Some(r.max_drawdown)),
            ("cagr", Some(r.cagr)),
            ("tracking_error", r.tracking_error),
            ("information_ratio", r.information_ratio),
            ("up_capture", r.up_capture),
            ("down_capture", r.down_capture),
            ("income", Some(r.income)),
            ("price", Some(r.price)),
            ("principal", Some(r.principal)),
            ("other", Some(r.other)),
            ("financing", Some(r.financing)),
            ("costs", Some(r.costs)),
            ("money_weighted", Some(r.money_weighted)),
        ] {
            d.set_item(k, v)?;
        }
        Ok(d)
    }

    fn __repr__(&self) -> String {
        format!(
            "Run({}, total_return={:.6}, trades={}, identity_error={:.1e})",
            self.name,
            self.out.total_return(),
            self.out.trades,
            self.out.max_identity_error
        )
    }
}

// ---------------------------------------------------------------------------------------- workspace

/// The workspace: series, entities, and the models' cached outputs.
#[pyclass(name = "Workspace", module = "fi_engine._native", subclass)]
pub struct PyWorkspace {
    ws: Workspace,
}

fn parse_known(k: Option<&Bound<'_, PyAny>>) -> PyResult<Known> {
    let Some(k) = k.filter(|k| !k.is_none()) else { return Ok(Known::Same) };
    if let Ok(c) = k.extract::<String>() {
        return Ok(Known::Column(c));
    }
    if let Ok((what, n)) = k.extract::<(String, i64)>() {
        return match what.as_str() {
            "lag" => Ok(Known::Lag(n)),
            "rows" => usize::try_from(n).map(Known::Rows).map_err(|_| PyValueError::new_err("a lag in rows is not negative")),
            _ => Err(PyValueError::new_err(format!("known is (\"lag\", nanoseconds) or (\"rows\", n), not {what}"))),
        };
    }
    Err(PyTypeError::new_err("known is a column name, fi.lag(...), or None (known at the event time)"))
}

fn parse_unit(u: &Bound<'_, PyAny>) -> PyResult<(Unit, f64, Option<String>)> {
    if let Ok((what, kind)) = u.extract::<(String, String)>() {
        return match what.as_str() {
            "ref" => Ok((Unit::Ref, 1.0, Some(kind))),
            _ => Err(PyValueError::new_err(format!("a unit is a name or (\"ref\", kind), not ({what}, {kind})"))),
        };
    }
    let name: String = u.extract().map_err(|_| PyTypeError::new_err("a unit is a name or fi.ref(kind)"))?;
    Ok(match name.as_str() {
        "number" => (Unit::Number, 1.0, None),
        "price" => (Unit::Price, 1.0, None),
        "rate" => (Unit::Rate, 1.0, None),
        "%" | "percent" => (Unit::Rate, 0.01, None),
        "bp" => (Unit::Rate, 1e-4, None),
        "probability" => (Unit::Probability, 1.0, None),
        "count" => (Unit::Count, 1.0, None),
        "category" => (Unit::Category, 1.0, Some("category".into())),
        "flag" => (Unit::Flag, 1.0, None),
        other => {
            return Err(PyValueError::new_err(format!(
                "unknown unit {other} (number, price, rate, %, bp, probability, count, category, flag, or fi.ref(kind))"
            )));
        }
    })
}

fn parse_missing(m: Option<&Bound<'_, PyAny>>) -> PyResult<MissingPolicy> {
    let Some(m) = m.filter(|m| !m.is_none()) else { return Ok(MissingPolicy::Fail) };
    if let Ok(s) = m.extract::<String>() {
        return match s.as_str() {
            "fail" => Ok(MissingPolicy::Fail),
            "carry" => Ok(MissingPolicy::Carry { max_age: None }),
            _ => Err(PyValueError::new_err(format!("missing is fail, carry, (\"carry\", max_days) or (\"default\", value), not {s}"))),
        };
    }
    let (what, v): (String, Option<f64>) = m.extract()?;
    match (what.as_str(), v) {
        ("carry", d) => Ok(MissingPolicy::Carry { max_age: d.map(days) }),
        ("default", Some(value)) => Ok(MissingPolicy::Default { value }),
        _ => Err(PyValueError::new_err("missing is fail, carry, (\"carry\", max_days) or (\"default\", value)")),
    }
}

impl PyWorkspace {
    fn entity_in(&self, series: &str, name: &str) -> PyResult<EntityId> {
        let s = self.ws.series(series).map_err(to_py)?;
        Names::new(&self.ws, &s.entities()).id(name)
    }
}

#[pymethods]
impl PyWorkspace {
    /// `information` is what decisions and models see of revised values: `"as_known"` (each
    /// vintage from its publication, the default), `"first_release"` (corrections never seen) or
    /// `"legacy_latest"` (revised values dated at first publication: not point in time).
    /// `outcomes` is which vintage contracts step on: `"as_booked"` (the default) or
    /// `"restated"`.
    #[new]
    #[pyo3(signature = (information = "as_known", outcomes = "as_booked"))]
    fn new(information: &str, outcomes: &str) -> PyResult<Self> {
        let information = match information {
            "as_known" => Information::AsKnown,
            "first_release" => Information::FirstRelease,
            "legacy_latest" => Information::LegacyLatest,
            i => return Err(PyValueError::new_err(format!("information is as_known, first_release or legacy_latest, not {i}"))),
        };
        let outcomes = match outcomes {
            "as_booked" => Outcomes::AsBooked,
            "restated" => Outcomes::Restated,
            o => return Err(PyValueError::new_err(format!("outcomes is as_booked or restated, not {o}"))),
        };
        Ok(Self { ws: Workspace::new(KnowledgePolicy { information, outcomes }) })
    }

    /// What decisions and models see of revised values.
    #[getter]
    fn information(&self) -> &'static str {
        match self.ws.knowledge().information {
            Information::AsKnown => "as_known",
            Information::FirstRelease => "first_release",
            Information::LegacyLatest => "legacy_latest",
        }
    }

    /// Which vintage defines outcomes.
    #[getter]
    fn outcomes(&self) -> &'static str {
        match self.ws.knowledge().outcomes {
            Outcomes::AsBooked => "as_booked",
            Outcomes::Restated => "restated",
        }
    }

    /// Where a series comes from: how its knowledge times were established, who produces it, and
    /// the state it describes.
    fn provenance<'py>(&self, py: Python<'py>, series: &str) -> PyResult<Bound<'py, PyDict>> {
        let s = self.ws.series(series).map_err(to_py)?;
        let p = s.provenance();
        let d = PyDict::new(py);
        match &p.knowledge {
            KnowledgeBasis::Observed => d.set_item("knowledge", "observed")?,
            KnowledgeBasis::Assumed(how) => {
                d.set_item("knowledge", "assumed")?;
                d.set_item("assumption", how)?;
            }
            KnowledgeBasis::Derived => d.set_item("knowledge", "derived")?,
        }
        match &p.producer {
            Producer::Observed => d.set_item("producer", "observation")?,
            Producer::Model { name, role } => {
                d.set_item("producer", format!("model {name}"))?;
                d.set_item("role", role_name(*role))?;
            }
            Producer::Book { name } => d.set_item("producer", format!("book {name}"))?,
        }
        d.set_item("describes", p.describes.clone())?;
        d.set_item("intervention", p.intervention.as_ref().map(|i| (i.kind(), i.by().to_owned())))?;
        d.set_item("revisions", s.has_revisions())?;
        Ok(d)
    }

    /// Registers a frame as a series, checked against its declaration. `data` is anything with
    /// `__arrow_c_stream__`; only the declared columns are read. Returns a summary.
    #[pyo3(signature = (name, data, *, entity = None, kind = None, time = None, known = None, values = None, grid = false, nan_is_missing = false, describes = None, scenario = None))]
    #[allow(clippy::too_many_arguments)]
    fn series<'py>(
        &mut self,
        py: Python<'py>,
        name: &str,
        data: PyTable,
        entity: Option<String>,
        kind: Option<String>,
        time: Option<String>,
        known: Option<&Bound<'py, PyAny>>,
        values: Option<&Bound<'py, PyDict>>,
        grid: bool,
        nan_is_missing: bool,
        describes: Option<String>,
        scenario: Option<String>,
    ) -> PyResult<Bound<'py, PyDict>> {
        let mut decl = SeriesDecl::new(name).known(parse_known(known)?);
        decl.scenario = scenario;
        if let Some(d) = &describes {
            decl = decl.describes(d);
        }
        let mut cols: Vec<String> = Vec::new();
        if let Some(e) = &entity {
            decl = decl.entity(e, kind.as_deref().unwrap_or(e));
            cols.push(e.clone());
        }
        if let Some(t) = &time {
            decl = decl.time(t);
            cols.push(t.clone());
        }
        if let Known::Column(c) = &decl.known {
            cols.push(c.clone());
        }
        for (k, u) in values.into_iter().flat_map(|v| v.iter()) {
            let column: String = k.extract()?;
            let (unit, scale, kind) = parse_unit(&u)?;
            cols.push(column.clone());
            decl.values.push(ValueSpec { column, unit, scale, kind });
        }
        if grid {
            decl = decl.grid();
        }
        if nan_is_missing {
            decl = decl.nan_is_missing();
        }
        let mut seen = HashSet::new();
        cols.retain(|c| seen.insert(c.clone()));
        let df = convert::frame(data, &cols.iter().map(String::as_str).collect::<Vec<_>>())?;
        let ws = &mut self.ws;
        let s = py.detach(|| fi_io::register(ws, &df, &decl)).map_err(to_py)?;
        let d = PyDict::new(py);
        d.set_item("name", name)?;
        d.set_item("rows", df.height())?;
        d.set_item("values", s.len())?;
        d.set_item("entities", s.entities().len())?;
        d.set_item("layout", if matches!(s.layout(), Layout::Grid { .. }) { "grid" } else { "rows" })?;
        d.set_item("version", s.version())?;
        Ok(d)
    }

    /// Series names.
    fn names(&self) -> Vec<String> {
        self.ws.names()
    }

    /// The entities a series describes.
    fn entities(&self, series: &str) -> PyResult<Vec<String>> {
        let s = self.ws.series(series).map_err(to_py)?;
        Ok(s.entities().iter().map(|e| self.ws.entities.name(*e).to_owned()).collect())
    }

    /// A series (registered or a model's output) as a long Polars frame.
    fn frame<'py>(&self, py: Python<'py>, series: &str) -> PyResult<Bound<'py, PyAny>> {
        let s = self.ws.series(series).map_err(to_py)?;
        convert::to_python(py, fi_io::series_frame(s, &self.ws.entities).map_err(convert::err)?)
    }

    /// A series' own event times, as a calendar (the data's own calendar).
    fn calendar(&self, series: &str) -> PyResult<PyCalendar> {
        let s = self.ws.series(series).map_err(to_py)?;
        let times = match s.layout() {
            Layout::Grid { times, .. } => times.clone(),
            Layout::Rows { event, .. } => event.iter().copied().filter(|t| *t != Ts::MIN).collect(),
        };
        Ok(PyCalendar { cal: Calendar::new(times) })
    }

    /// Evaluates models over `calendar`, in dependency order, in parallel where they allow it.
    /// Outputs are cached by their inputs' versions: a second evaluation is free.
    /// `substitute` maps a state (a series a model writes) to a registered observation of it,
    /// to replay history: refused where anything upstream of the state is intervened. The
    /// report names what was evaluated, cached, substituted and intervened, and the mode.
    #[pyo3(signature = (models, calendar, *, substitute = None))]
    fn evaluate<'py>(
        &mut self,
        py: Python<'py>,
        models: Vec<PyRef<'py, PyModel>>,
        calendar: &PyCalendar,
        substitute: Option<HashMap<String, String>>,
    ) -> PyResult<Bound<'py, PyDict>> {
        let names = Arc::new((0..self.ws.entities.len()).map(|i| self.ws.entities.name(EntityId(i as u32)).to_owned()).collect::<Vec<_>>());
        let mut g = Graph::new();
        let mut pythons = Vec::new();
        for m in &models {
            if let Some(p) = &m.python {
                *p.names.write().expect("not poisoned") = names.clone();
                pythons.push(p.clone());
            }
            g.add(m.model.clone());
        }
        for (state, obs) in substitute.iter().flatten() {
            g.substitute(state, obs);
        }
        let (ws, cal) = (&mut self.ws, &calendar.cal);
        let report = py.detach(|| g.evaluate(ws, cal)).map_err(|e| {
            let mut errors: Vec<&Errors> = pythons.iter().map(|p| &p.errors).collect();
            errors.extend(models.iter().flat_map(|m| m.errors.iter().map(AsRef::as_ref)));
            raised(e, &errors)
        })?;
        let d = PyDict::new(py);
        d.set_item("mode", report.mode().to_string())?;
        d.set_item("substituted", report.substituted.iter().cloned().collect::<HashMap<String, String>>())?;
        let intervened = PyDict::new(py);
        for (m, i) in &report.intervened {
            intervened.set_item(m, (i.kind(), i.by()))?;
        }
        d.set_item("intervened", intervened)?;
        d.set_item("evaluated", report.evaluated)?;
        d.set_item("cached", report.cached)?;
        Ok(d)
    }

    /// Bonds (or any unit) priced from supplied marks: one contract per entity of `marks`
    /// (or the named ones). One unit is one unit of original face; marks are per 100 of current
    /// face; `flows` (if given) holds `interest`, `principal_face`, `principal_price`,
    /// `writedown_face`, `writeup_face` and `pik_face`. A seasoned bond names its `factor` column
    /// in the marks, read at the first time.
    /// `currency` names the bonds' currency (default: the book's base). With `reconcile`, a coupon
    /// the accrued interest does not fall back from is refused, since it would count twice; turn
    /// it off only for steps as long as a coupon period.
    #[pyo3(signature = (marks, *, clean = "clean", accrued = "accrued", flows = None, factor = None, entities = None, currency = None, reconcile = true))]
    #[allow(clippy::too_many_arguments)]
    fn supplied_bonds(
        &mut self,
        marks: &str,
        clean: &str,
        accrued: &str,
        flows: Option<String>,
        factor: Option<String>,
        entities: Option<Vec<String>>,
        currency: Option<&str>,
        reconcile: bool,
    ) -> PyResult<PyContracts> {
        let ccy = currency.map(|c| self.ws.entities.intern("currency", c));
        let s = self.ws.series(marks).map_err(to_py)?;
        s.column(clean).map_err(to_py)?;
        s.column(accrued).map_err(to_py)?;
        if let Some(f) = &factor {
            s.column(f).map_err(to_py)?;
        }
        if let Some(f) = &flows {
            self.ws.series(f).map_err(to_py)?;
        }
        let ids: Vec<EntityId> = match entities {
            None => s.entities(),
            Some(n) => {
                let own = Names::new(&self.ws, &s.entities());
                n.iter().map(|x| own.id(x)).collect::<PyResult<_>>()?
            }
        };
        let inner = ids
            .into_iter()
            .map(|e| {
                let bond = SuppliedBond {
                    entity: e,
                    marks: marks.into(),
                    clean: clean.into(),
                    accrued: accrued.into(),
                    flows: flows.clone(),
                    factor: factor.clone(),
                    reconcile,
                };
                in_currency(Arc::new(bond), ccy)
            })
            .collect();
        Ok(PyContracts { inner })
    }

    /// Debt contracts from terms: `bonds` maps each instrument's name to its `Bond`. `valuation`
    /// says where clean prices come from (`fi.valuation.clean`, `.yield_` or `.curve`).
    /// `fixings` (`series.column`) is where floating and inflation coupons fix, keyed by an entity
    /// named as the coupon's index. `events` names a series of observed lifecycle events keyed by
    /// instrument, with any of the columns `call` and `put` (price per 100), `default` (1),
    /// `recovery` (cash per 100 of face), `writedown` and `writeup` (face per unit of original
    /// face), `factor` (face outstanding, as reported) and `extend` (1). The instruments are
    /// entities of `kind`; each is held in its `Bond`'s currency (default: the book's base).
    #[pyo3(signature = (bonds, valuation, *, fixings = None, events = None, kind = "bond"))]
    fn debt(
        &mut self,
        bonds: &Bound<'_, PyDict>,
        valuation: &Bound<'_, PyAny>,
        fixings: Option<&str>,
        events: Option<&str>,
        kind: &str,
    ) -> PyResult<PyContracts> {
        Ok(PyContracts { inner: terms::debt_contracts(&mut self.ws, bonds, valuation, fixings, events, kind)? })
    }

    /// A tradable level (an index, a total-return series): one unit is worth the level, in
    /// `currency` (default: the book's base).
    #[pyo3(signature = (entity, level, *, currency = None))]
    fn total_return(&mut self, entity: &str, level: &str, currency: Option<&str>) -> PyResult<PyContracts> {
        let (series, column) = split(level)?;
        self.ws.series(series).and_then(|s| s.column(column)).map_err(to_py)?;
        let e = self.entity_in(series, entity)?;
        let ccy = currency.map(|c| self.ws.entities.intern("currency", c));
        Ok(PyContracts { inner: vec![in_currency(Arc::new(TotalReturn { entity: e, series: series.into(), column: column.into() }), ccy)] })
    }

    /// A cash account, accruing `rate` (`series.column`, a market-wide rate) in `day_count` if
    /// given.
    #[pyo3(signature = (name = "cash", rate = None, *, day_count = "ACT/360"))]
    fn cash_account(&mut self, name: &str, rate: Option<&str>, day_count: &str) -> PyResult<PyContracts> {
        let day_count: fi_kernel::DayCount = day_count.parse().map_err(|e: fi_terms::Error| PyValueError::new_err(e.to_string()))?;
        let rate = rate.map(split).transpose()?.map(|(s, c)| (s.to_owned(), c.to_owned()));
        if let Some((s, c)) = &rate {
            self.ws.series(s).and_then(|x| x.column(c)).map_err(to_py)?;
        }
        let entity = self.ws.entities.intern("cash", name);
        Ok(PyContracts { inner: vec![Arc::new(CashAccount { entity, rate, day_count })] })
    }

    /// A future, marked to market daily: one unit gains `multiplier` × the price change, paid as
    /// variation margin in `currency` (default: the book's base).
    #[pyo3(signature = (entity, price, multiplier, *, currency = None))]
    fn daily_future(&mut self, entity: &str, price: &str, multiplier: f64, currency: Option<&str>) -> PyResult<PyContracts> {
        let (series, column) = split(price)?;
        self.ws.series(series).and_then(|s| s.column(column)).map_err(to_py)?;
        let e = self.entity_in(series, entity)?;
        let ccy = currency.map(|c| self.ws.entities.intern("currency", c));
        Ok(PyContracts { inner: vec![in_currency(Arc::new(DailyFuture { entity: e, series: series.into(), column: column.into(), multiplier }), ccy)] })
    }

    /// An exchange-traded future, its terms a Nautilus `FuturesContract` built from these fields
    /// and checked by Nautilus: `symbol` on `venue`, of `underlying`, listed at `activation` and
    /// expiring at `expiration`, in `currency`, with its tick (`price_increment`, whose decimals
    /// are its precision), `multiplier` and `lot_size`. One unit is one contract, marked to market
    /// each step at `price` (`series.column`, keyed by `name`), so its P&L is variation margin.
    #[pyo3(signature = (name, price, *, expiration, multiplier, price_increment, activation = None, underlying = None, venue = "XCME", currency = "USD", lot_size = 1.0, asset_class = "debt"))]
    #[allow(clippy::too_many_arguments)]
    fn future(
        &mut self,
        name: &str,
        price: &str,
        expiration: &Bound<'_, PyAny>,
        multiplier: f64,
        price_increment: &str,
        activation: Option<&Bound<'_, PyAny>>,
        underlying: Option<&str>,
        venue: &str,
        currency: &str,
        lot_size: f64,
        asset_class: &str,
    ) -> PyResult<PyContracts> {
        let (series, column) = split(price)?;
        self.ws.series(series).and_then(|s| s.column(column)).map_err(to_py)?;
        let e = self.entity_in(series, name)?;
        let instrument = book::futures_contract(
            name,
            venue,
            underlying.unwrap_or(name),
            activation,
            expiration,
            currency,
            price_increment,
            multiplier,
            lot_size,
            asset_class,
        )?;
        let ccy = Some(self.ws.entities.intern("currency", currency));
        let c = fi_exec::ExchangeFuture { entity: e, instrument, price: (series.into(), column.into()), currency: ccy };
        Ok(PyContracts { inner: vec![Arc::new(c)] })
    }

    /// The run's manifest: everything its output depends on besides the data's values (series
    /// versions and provenance, the knowledge policy, models with their fingerprints, roles and
    /// vintages, the calendar, the book's policies and settings, the mode, approximations), and
    /// the output's version (spec, change 7).
    fn manifest(&mut self, py: Python<'_>, run: &PyRun, book: &PyBook, records: &PyRecords) -> PyResult<PyManifest> {
        let (b, _) = book.build(py, &self.ws, records)?;
        Ok(PyManifest { m: fi_research::Manifest::of(&self.ws, &records.rec, &records.reads, &b, &run.out, &records.approximations) })
    }

    /// A run's NAV against an official one (`"series.column"`, for `entity` or market-wide):
    /// date, engine, official and difference. Nothing is plugged: what the engine does not
    /// explain stays visible.
    #[pyo3(signature = (run, official, *, entity = None))]
    fn reconcile<'py>(&self, py: Python<'py>, run: &PyRun, official: &str, entity: Option<&str>) -> PyResult<Bound<'py, PyAny>> {
        let (s, c) = split(official)?;
        let e = match entity {
            Some(n) => self.entity_in(s, n)?,
            None => EntityId::NONE,
        };
        let r = fi_research::reconcile(&self.ws, &run.out, s, c, e).map_err(to_py)?;
        let d = PyDict::new(py);
        d.set_item("date", r.times.iter().map(|t| convert::date(py, *t)).collect::<PyResult<Vec<_>>>()?)?;
        d.set_item("engine", r.engine)?;
        d.set_item("official", r.official.iter().map(|x| x.is_finite().then_some(*x)).collect::<Vec<_>>())?;
        d.set_item("difference", r.difference.iter().map(|x| x.is_finite().then_some(*x)).collect::<Vec<_>>())?;
        py.import("polars")?.call_method1("DataFrame", (d,))
    }

    /// A world: a copy of this workspace that shares every series and cached output with it by
    /// reference (spec, change 6). Evaluate what does not vary by world here first; then each
    /// world evaluates its own stochastic models.
    fn world(&self) -> Self {
        Self { ws: self.ws.clone() }
    }

    /// Makes `other` a world of this workspace (for subclasses: `fi.Workspace.world`).
    fn _share_into(&self, mut other: PyRefMut<'_, Self>) {
        other.ws = self.ws.clone();
    }

    /// Correlated Gaussian paths, a real-world model writing series `name` (column `value`):
    /// an Ornstein–Uhlenbeck process for each entity of `start` (`{name: first value}`, of
    /// `kind`; a number for one market-wide series), reverting at `reversion` per year to
    /// `mean` (or drifting at `drift` without reversion), with volatility `vol` per square-root
    /// year and shocks loading `correlation` on a common factor. Draws are keyed by `seed`,
    /// `world`, entity and time.
    #[pyo3(signature = (name, start, *, kind = "index", mean = 0.0, reversion = 0.0, drift = 0.0, vol = 0.01, correlation = 0.0, floor = None, seed = 0, world = 0))]
    #[allow(clippy::too_many_arguments)]
    fn gaussian_paths(
        &mut self,
        name: &str,
        start: &Bound<'_, PyAny>,
        kind: &str,
        mean: f64,
        reversion: f64,
        drift: f64,
        vol: f64,
        correlation: f64,
        floor: Option<f64>,
        seed: u64,
        world: u64,
    ) -> PyResult<PyModel> {
        let start = if let Ok(x) = start.extract::<f64>() {
            vec![(EntityId::NONE, x)]
        } else {
            let d = start.cast::<PyDict>().map_err(|_| PyTypeError::new_err("start is a number (market-wide) or {name: first value}"))?;
            d.iter().map(|(k, v)| Ok((self.ws.entities.intern(kind, &k.extract::<String>()?), v.extract::<f64>()?))).collect::<PyResult<_>>()?
        };
        let m = GaussianPaths { name: name.into(), start, mean, reversion, drift, vol, correlation, floor, seed, world };
        Ok(PyModel { model: fi_kernel::Model::batch(m), python: None, errors: Vec::new() })
    }

    /// Loan cash flows in a world with correlated defaults, a real-world model writing the rows
    /// series `name` (`interest`, `principal_face`, `writedown_face`, `defaulted`, per unit of
    /// face) for `supplied_bonds` loans. `loans` lists `(loan, borrower, coupon, maturity,
    /// hazard, recovery)`: one borrower's loans default together, each recovering its own share.
    /// Defaults draw exponential times through a one-factor Gaussian copula with `correlation`,
    /// recognised on the next of `payments`.
    #[pyo3(signature = (name, loans, payments, *, correlation = 0.0, seed = 0, world = 0))]
    fn defaults(
        &mut self,
        name: &str,
        loans: Vec<(String, String, f64, Bound<'_, PyAny>, f64, f64)>,
        payments: Vec<Bound<'_, PyAny>>,
        correlation: f64,
        seed: u64,
        world: u64,
    ) -> PyResult<PyModel> {
        let loans = loans
            .into_iter()
            .map(|(l, b, coupon, maturity, hazard, recovery)| {
                Ok(LoanTerms {
                    entity: self.ws.entities.intern("loan", &l),
                    borrower: self.ws.entities.intern("borrower", &b),
                    coupon,
                    maturity: convert::ts(&maturity)?,
                    hazard,
                    recovery,
                })
            })
            .collect::<PyResult<_>>()?;
        let payments = payments.iter().map(convert::ts).collect::<PyResult<_>>()?;
        let m = Defaults { name: name.into(), loans, payments, correlation, seed, world };
        Ok(PyModel { model: fi_kernel::Model::batch(m), python: None, errors: Vec::new() })
    }

    /// A deal (a securitisation): a collateral book and its priority of payments, as a model.
    /// Evaluating it on `calendar` writes `{name}.flows`, `.marks`, `.distributions`, `.tests`,
    /// `.accounts`, `.collateral` and `.claims`; its tranches are then contracts
    /// (`ws.tranches(name)`). The terms come from `fi_engine.deals`; steps are strings such as
    /// `"interest:A"` or `"sequential:A,B when amortising"`. Entities are named `{name}.{part}`.
    ///
    /// `rounding` is the indenture's: `(unit or currency, mode)`, such as `("USD", "half_up")`.
    /// Every amount due and paid is then whole minor units, a pro rata split gives each odd unit
    /// to the largest remainder, and a cure is the smallest whole amount that cures. Default: none.
    #[pyo3(signature = (
        name, calendar, *, collateral, opening, tranches, interest, principal = Vec::new(), payments = None, fees = None, tests = None, haircuts = None,
        triggers = None, losses = None, cash = 0.0, reserve = 0.0, reserve_target = 0.0, reinvest_until = None, maturity = None, rights = None,
        exercises = None, manager = None, fill = None, missing = None, rounding = None, reads = Vec::new()
    ))]
    #[allow(clippy::too_many_arguments)]
    fn deal<'py>(
        &mut self,
        py: Python<'py>,
        name: &str,
        calendar: &PyCalendar,
        collateral: &PyContracts,
        opening: Bound<'py, PyDict>,
        tranches: Bound<'py, PyAny>,
        interest: Vec<String>,
        principal: Vec<String>,
        payments: Option<Bound<'py, PyAny>>,
        fees: Option<Bound<'py, PyAny>>,
        tests: Option<Bound<'py, PyAny>>,
        haircuts: Option<Bound<'py, PyAny>>,
        triggers: Option<Bound<'py, PyAny>>,
        losses: Option<Bound<'py, PyAny>>,
        cash: f64,
        reserve: f64,
        reserve_target: f64,
        reinvest_until: Option<Bound<'py, PyAny>>,
        maturity: Option<Bound<'py, PyAny>>,
        rights: Option<Bound<'py, PyAny>>,
        exercises: Option<Bound<'py, PyAny>>,
        manager: Option<Bound<'py, PyAny>>,
        fill: Option<PyRef<'py, PyFill>>,
        missing: Option<Bound<'py, PyAny>>,
        rounding: Option<Bound<'py, PyAny>>,
        reads: Vec<String>,
    ) -> PyResult<PyModel> {
        let args = deal::DealArgs {
            name: name.to_owned(),
            calendar: calendar.cal.clone(),
            collateral: collateral.inner.clone(),
            opening,
            tranches,
            payments,
            interest,
            principal,
            fees,
            tests,
            haircuts,
            triggers,
            losses,
            cash,
            reserve,
            reserve_target,
            reinvest_until,
            maturity,
            rights,
            exercises,
            manager,
            fill,
            missing: parse_missing(missing.as_ref())?,
            rounding: book::parse_rounding(rounding.as_ref())?,
            reads,
        };
        let (d, errors) = deal::build(py, &mut self.ws, args)?;
        Ok(PyModel { model: fi_kernel::Model::batch(d), python: None, errors })
    }

    /// Registers a currency (an entity of kind `currency`), so it can be a book's base.
    fn currency(&mut self, name: &str) {
        self.ws.entities.intern("currency", name);
    }

    /// Steps `contracts` over `calendar` into unit records. `missing` is `"fail"` (default),
    /// `"carry"`, `("carry", max_days)` or `("default", value)`; whatever is carried or
    /// defaulted is counted. `requires` maps a contract kind to the features its contracts must
    /// model (`{"supplied_bond": ["writedown"]}`); a gap is refused unless `approximate` declares
    /// it, and declared approximations go into the run's manifest. `transitions`
    /// (`fi.transition(...)`) move every holder of a contract into others and cash at an event: an
    /// exchange, a conversion, the settlement of a claim.
    #[pyo3(signature = (contracts, calendar, missing = None, *, requires = None, approximate = None, transitions = None))]
    #[allow(clippy::too_many_arguments)]
    fn records(
        &self,
        py: Python<'_>,
        contracts: &PyContracts,
        calendar: &PyCalendar,
        missing: Option<&Bound<'_, PyAny>>,
        requires: Option<HashMap<String, Vec<String>>>,
        approximate: Option<HashMap<String, Vec<String>>>,
        transitions: Option<Vec<Bound<'_, PyAny>>>,
    ) -> PyResult<PyRecords> {
        let policy = parse_missing(missing)?;
        let pairs = |m: Option<HashMap<String, Vec<String>>>| -> Vec<(String, String)> {
            let mut v: Vec<(String, String)> = m.into_iter().flatten().flat_map(|(k, fs)| fs.into_iter().map(move |f| (k.clone(), f))).collect();
            v.sort();
            v
        };
        let approximations = fi_research::check_requirements(&self.ws, &contracts.inner, &pairs(requires), &pairs(approximate)).map_err(to_py)?;
        let (ws, c, cal) = (&self.ws, &contracts.inner, &calendar.cal);
        let (mut rec, d) = py.detach(|| UnitRecords::compute(c, ws, cal, policy)).map_err(to_py)?;
        let names = Arc::new(Names::new(&self.ws, rec.entities()));
        if let Some(list) = transitions {
            let parsed = list.iter().map(|x| transition(x, &names)).collect::<PyResult<Vec<_>>>()?;
            rec.with_transitions(parsed, &self.ws.entities).map_err(to_py)?;
        }
        let mut reads: Vec<String> = c.iter().flat_map(|x| x.reads()).collect();
        reads.sort();
        reads.dedup();
        Ok(PyRecords { rec: Arc::new(rec), names, diagnostics: d.snapshot(), reads: Arc::new(reads), approximations: Arc::new(approximations) })
    }

    /// Each contract's P&L per unit explained at each time (review M7): `total`, and where the
    /// contract explains itself (a debt valued off a curve), `carry` (the accrual, the flows and
    /// the roll down the curve, with the market unchanged in tenor), `curve` and `spread`, which
    /// add up to it; `residual` is what they leave (all of it where nothing explains it). Steps
    /// the contracts again; nothing is kept in the records. `fi.attribution` weights it by a run's
    /// holdings.
    #[pyo3(signature = (contracts, calendar, missing = None))]
    fn explain<'py>(&self, py: Python<'py>, contracts: &PyContracts, calendar: &PyCalendar, missing: Option<&Bound<'_, PyAny>>) -> PyResult<Bound<'py, PyAny>> {
        let policy = parse_missing(missing)?;
        let (ws, c, cal) = (&self.ws, &contracts.inner, &calendar.cal);
        let x = py.detach(|| fi_kernel::Explained::compute(c, ws, cal, policy)).map_err(to_py)?;
        convert::to_python(py, fi_io::explained_frame(&x, cal, &self.ws.entities).map_err(convert::err)?)
    }

    /// The records plus a composite: a portfolio of `legs` held as one contract. `schedule` lists
    /// `(time, value weights)`; the first must be at or before the calendar's first time; later
    /// entries are rolls, each costing `roll_cost_bp` of the value traded.
    #[pyo3(signature = (records, name, legs, schedule, roll_cost_bp = 0.0))]
    fn composite(
        &mut self,
        records: &PyRecords,
        name: &str,
        legs: Vec<String>,
        schedule: Vec<(Bound<'_, PyAny>, Vec<f64>)>,
        roll_cost_bp: f64,
    ) -> PyResult<PyRecords> {
        let legs = legs.iter().map(|l| records.names.id(l)).collect::<PyResult<_>>()?;
        let schedule = schedule.iter().map(|(t, w)| Ok((convert::ts(t)?, w.clone()))).collect::<PyResult<_>>()?;
        let entity = self.ws.entities.intern("composite", name);
        let (built, _) = Composite { entity, legs, schedule, roll_cost_bp }.build(&records.rec).map_err(to_py)?;
        let mut rec = (*records.rec).clone();
        rec.extend(&built).map_err(to_py)?;
        let names = Arc::new(Names::new(&self.ws, rec.entities()));
        Ok(PyRecords {
            rec: Arc::new(rec),
            names,
            diagnostics: records.diagnostics,
            reads: records.reads.clone(),
            approximations: records.approximations.clone(),
        })
    }

    /// Runs a book (or a strategy) on the records.
    fn run(&mut self, py: Python<'_>, book: &PyBook, records: &PyRecords) -> PyResult<PyRun> {
        if let Some(c) = book.currency() {
            self.ws.entities.intern("currency", c);
        }
        let entities = Arc::new(self.ws.entities.clone());
        let (b, errors) = book.build(py, &self.ws, records)?;
        let (ws, rec) = (&self.ws, records.rec.as_ref());
        let out = py.detach(|| fi_research::run_book(ws, rec, &b));
        finish(out, &errors, records, &entities, &b, &self.ws)
    }

    /// Runs many books in parallel on the same records; results in input order.
    fn sweep(&mut self, py: Python<'_>, books: Vec<PyRef<'_, PyBook>>, records: &PyRecords) -> PyResult<Vec<PyRun>> {
        for b in &books {
            if let Some(c) = b.currency() {
                self.ws.entities.intern("currency", c);
            }
        }
        let entities = Arc::new(self.ws.entities.clone());
        let built: Vec<(Book, Vec<Arc<Errors>>)> = books.iter().map(|b| b.build(py, &self.ws, records)).collect::<PyResult<_>>()?;
        let list: Vec<Book> = built.iter().map(|(b, _)| b.clone()).collect();
        let (ws, rec) = (&self.ws, records.rec.as_ref());
        let outs = py.detach(|| fi_research::sweep_books(ws, rec, &list));
        outs.into_iter().zip(&built).map(|(o, (b, e))| finish(o, e, records, &entities, b, &self.ws)).collect()
    }

    /// Runs several books in lockstep through one shared `execution`, which sees every book's
    /// orders together: the books compete for its capacity (accounts that must be implementable
    /// at the same time). Each book's own execution is ignored.
    fn run_together(&mut self, py: Python<'_>, books: Vec<PyRef<'_, PyBook>>, records: &PyRecords, execution: &PyExecution) -> PyResult<Vec<PyRun>> {
        for b in &books {
            if let Some(c) = b.currency() {
                self.ws.entities.intern("currency", c);
            }
        }
        let entities = Arc::new(self.ws.entities.clone());
        let built: Vec<(Book, Vec<Arc<Errors>>)> = books.iter().map(|b| b.build(py, &self.ws, records)).collect::<PyResult<_>>()?;
        let list: Vec<Book> = built.iter().map(|(b, _)| b.clone()).collect();
        let shared = execution.build(&records.names)?;
        let errors: Vec<Arc<Errors>> = built.iter().flat_map(|(_, e)| e.iter().cloned()).collect();
        let (ws, rec) = (&self.ws, records.rec.as_ref());
        let outs = py
            .detach(|| fi_research::run_together(ws, rec, &list, shared.as_ref()))
            .map_err(|e| raised(e, &errors.iter().map(AsRef::as_ref).collect::<Vec<_>>()))?;
        outs.into_iter().zip(&built).map(|(o, (b, e))| finish(Ok(o), e, records, &entities, b, &self.ws)).collect()
    }

    /// A synthetic universe (the one the Rust tests and speed tests use): bonds with marks,
    /// accrued and flows, issuers with equity and filed financials, governments, an index, a rate.
    #[pyo3(signature = (*, bonds = 200, days = 520, seed = 7, issuers = 20, govts = 30, amortising_every = 5, pik_every = 11, gap_rate = 0.0))]
    #[allow(clippy::too_many_arguments)]
    fn synthetic<'py>(
        &mut self,
        py: Python<'py>,
        bonds: usize,
        days: usize,
        seed: u64,
        issuers: usize,
        govts: usize,
        amortising_every: usize,
        pik_every: usize,
        gap_rate: f64,
    ) -> PyResult<Bound<'py, PyDict>> {
        let cfg = synth::Config { bonds, days, seed, amortising_every, pik_every, gap_rate, issuers, govts };
        let ws = &mut self.ws;
        let u = py.detach(|| synth::build(ws, &cfg)).map_err(to_py)?;
        let d = PyDict::new(py);
        d.set_item("calendar", Bound::new(py, PyCalendar { cal: u.calendar.clone() })?)?;
        d.set_item("bonds", Bound::new(py, PyContracts { inner: u.bond_contracts() })?)?;
        d.set_item("index", Bound::new(py, PyContracts { inner: vec![u.index_contract()] })?)?;
        d.set_item("gaps", u.gaps)?;
        Ok(d)
    }

    fn __repr__(&self) -> String {
        format!(
            "Workspace({} series, {} entities, information={}, outcomes={})",
            self.ws.names().len(),
            self.ws.entities.len(),
            self.information(),
            self.outcomes()
        )
    }
}

#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyWorkspace>()?;
    m.add_class::<PyCalendar>()?;
    m.add_class::<PyInput>()?;
    m.add_class::<PyModel>()?;
    m.add_class::<PyContracts>()?;
    m.add_class::<PyRecords>()?;
    m.add_class::<PyDecisionSpec>()?;
    m.add_class::<PyFill>()?;
    m.add_class::<PyPolicy>()?;
    m.add_class::<PyBook>()?;
    m.add_class::<PyExecution>()?;
    m.add_class::<PyConstraint>()?;
    m.add_class::<PyAgreement>()?;
    m.add_class::<PyManifest>()?;
    m.add_class::<PyRun>()?;
    m.add_class::<bridge::Context>()?;
    m.add_class::<terms::PyBond>()?;
    m.add_class::<terms::PyCoupon>()?;
    m.add_class::<terms::PyBusinessCalendar>()?;
    m.add_class::<terms::PyCurve>()?;
    let py = m.py();
    m.add("EngineError", py.get_type::<EngineError>())?;
    m.add("InvalidError", py.get_type::<InvalidError>())?;
    m.add("UnknownError", py.get_type::<UnknownError>())?;
    m.add("MissingError", py.get_type::<MissingError>())?;
    m.add("DAY", Ts::DAY)?;
    // Under a supervisor, the engine reports its progress into a ring of its own, closed at exit.
    progress::install();
    m.add_function(wrap_pyfunction!(progress::_close_progress, m)?)?;
    py.import("atexit")?.call_method1("register", (m.getattr("_close_progress")?,))?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
