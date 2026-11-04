//! Models: declared, point-in-time transforms from series to series (axiom 6).
//!
//! A model declares its inputs, outputs and shape; the [`crate::graph::Graph`] runs it.
//!
//! * [`CrossSectionModel`]: one call per time, across entities (curve fits, z-scores, ranks).
//! * [`TimeSeriesModel`]: one call per entity per time, in time order, with its own state
//!   (Merton, volatility, EWMA). It sees only the current inputs, so it is causal by
//!   construction, as Nautilus's indicators are.
//!
//! For an output at event time `t`, inputs are read with event time at or before `t` and
//! knowledge time at or before `t + window`. The output is known at `t + window + lag`, so it is
//! never known before the inputs it used (invariant I9).

use std::sync::Arc;

use crate::{
    Error, Result,
    entity::EntityId,
    hash::Hash64,
    series::{Intervention, ModelRole, Series},
    time::{Calendar, Ts},
    workspace::Workspace,
};

/// One input: a column of a series, read exactly at the event time or as of it, optionally
/// through a relationship (for example a bond's issuer).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Input {
    /// Series name.
    pub series: String,
    /// Column name.
    pub column: String,
    /// Read the latest value at or before the event time (slow data), not exactly at it.
    pub asof: bool,
    /// Read the value of the related entity: (relationship series, reference column).
    pub via: Option<(String, String)>,
    /// Read a market-wide series (entity [`EntityId::NONE`]) for every entity: a cash rate.
    pub market: bool,
}

impl Input {
    /// Read the value at exactly the event time.
    #[must_use]
    pub fn exact(series: &str, column: &str) -> Self {
        Self { series: series.to_owned(), column: column.to_owned(), asof: false, via: None, market: false }
    }

    /// Read the latest value at or before the event time.
    #[must_use]
    pub fn asof(series: &str, column: &str) -> Self {
        Self { asof: true, ..Self::exact(series, column) }
    }

    /// Read the latest value of a market-wide series (no entity) at or before the event time.
    #[must_use]
    pub fn market(series: &str, column: &str) -> Self {
        Self { market: true, ..Self::asof(series, column) }
    }

    /// Read through relationship column `column` of `series`.
    #[must_use]
    pub fn via(mut self, series: &str, column: &str) -> Self {
        self.via = Some((series.to_owned(), column.to_owned()));
        self
    }
}

/// How a model's work divides.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub enum Shape {
    /// Per time, across entities.
    #[default]
    CrossSection,
    /// Per entity, across time.
    TimeSeries,
    /// Whole series at once over the calendar: a book or a deal, which steps its own state.
    Batch,
}

/// A model's declaration.
#[derive(Clone, Debug)]
pub struct ModelSpec {
    /// Name: the output series is called this.
    pub name: String,
    /// Inputs, in the order the model receives them.
    pub inputs: Vec<Input>,
    /// Output column names.
    pub outputs: Vec<String>,
    /// Shape.
    pub shape: Shape,
    /// The series whose entities form the universe (default: the first input without `via`).
    pub universe: Option<String>,
    /// How long after an event time its inputs may be published and still be used (ns).
    pub window: i64,
    /// Compute lag added to the output's knowledge time (ns).
    pub lag: i64,
    /// Hash of the model's parameters (part of the cache key).
    pub params: u64,
    /// Whether the graph may evaluate it on several threads (false for Python models).
    pub parallel: bool,
    /// What the model is for (spec, change 6).
    pub role: ModelRole,
    /// When the model became available. A point-in-time run has no output before then.
    pub available_from: Option<Ts>,
    /// Use the model before it was available: a labelled retrospective experiment.
    pub retrospective: bool,
    /// The state its output describes (spec, change 4); by default its own name.
    pub describes: Option<String>,
    /// Series read besides the inputs (a batch model's dependencies).
    pub reads: Vec<String>,
    /// Series a batch model writes (the others write one series, named after the model).
    pub writes: Vec<String>,
}

impl ModelSpec {
    /// A spec with a one-day window, no lag, and parallel evaluation.
    #[must_use]
    pub fn new(name: &str, shape: Shape) -> Self {
        Self {
            name: name.to_owned(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            shape,
            universe: None,
            window: Ts::DAY,
            lag: 0,
            params: 0,
            parallel: true,
            role: ModelRole::Signal,
            available_from: None,
            retrospective: false,
            describes: None,
            reads: Vec::new(),
            writes: Vec::new(),
        }
    }

    /// Adds an input.
    #[must_use]
    pub fn input(mut self, input: Input) -> Self {
        self.inputs.push(input);
        self
    }

    /// Adds an output column.
    #[must_use]
    pub fn output(mut self, name: &str) -> Self {
        self.outputs.push(name.to_owned());
        self
    }

    /// Sets the universe series.
    #[must_use]
    pub fn universe(mut self, series: &str) -> Self {
        self.universe = Some(series.to_owned());
        self
    }

    /// Sets the publication window (ns).
    #[must_use]
    pub fn window(mut self, nanos: i64) -> Self {
        self.window = nanos;
        self
    }

    /// Sets the compute lag (ns).
    #[must_use]
    pub fn lag(mut self, nanos: i64) -> Self {
        self.lag = nanos;
        self
    }

    /// Sets the parameter hash.
    #[must_use]
    pub fn params(mut self, h: u64) -> Self {
        self.params = h;
        self
    }

    /// Evaluates on one thread only.
    #[must_use]
    pub fn serial(mut self) -> Self {
        self.parallel = false;
        self
    }

    /// Sets the model's role.
    #[must_use]
    pub fn role(mut self, role: ModelRole) -> Self {
        self.role = role;
        self
    }

    /// Declares when the model became available.
    #[must_use]
    pub fn available_from(mut self, t: Ts) -> Self {
        self.available_from = Some(t);
        self
    }

    /// Uses the model before it was available, as a labelled retrospective experiment.
    #[must_use]
    pub fn retrospective(mut self) -> Self {
        self.retrospective = true;
        self
    }

    /// Declares the state the output describes.
    #[must_use]
    pub fn describes(mut self, state: &str) -> Self {
        self.describes = Some(state.to_owned());
        self
    }

    /// Declares a series read besides the inputs.
    #[must_use]
    pub fn reads(mut self, series: &str) -> Self {
        self.reads.push(series.to_owned());
        self
    }

    /// Declares a series a batch model writes.
    #[must_use]
    pub fn writes(mut self, series: &str) -> Self {
        self.writes.push(series.to_owned());
        self
    }

    /// The series the model produces.
    #[must_use]
    pub fn produced(&self) -> Vec<String> {
        if self.shape == Shape::Batch { self.writes.clone() } else { vec![self.name.clone()] }
    }

    /// Hash of everything that changes the output except the inputs' content.
    #[must_use]
    pub fn fingerprint(&self) -> u64 {
        let mut h = Hash64::new();
        h.str(&self.name);
        h.u64(self.shape as u64);
        for i in &self.inputs {
            h.str(&i.series);
            h.str(&i.column);
            h.u64(u64::from(i.asof));
            h.u64(u64::from(i.market));
            if let Some((s, c)) = &i.via {
                h.str(s);
                h.str(c);
            }
        }
        for o in &self.outputs {
            h.str(o);
        }
        h.str(self.universe.as_deref().unwrap_or(""));
        h.i64(self.window);
        h.i64(self.lag);
        h.u64(self.params);
        h.u64(self.role as u64);
        h.i64(self.available_from.map_or(i64::MIN, |t| t.0));
        h.u64(u64::from(self.retrospective));
        for s in self.reads.iter().chain(&self.writes) {
            h.str(s);
        }
        h.finish()
    }
}

/// A cross-sectional model: one call per time.
pub trait CrossSectionModel: Send + Sync {
    /// The declaration.
    fn spec(&self) -> ModelSpec;
    /// `x[i][j]` is input `i` for universe entity `j` (NaN when missing). Returns one vector per
    /// output, each as long as the universe. An error stops the evaluation (principle 7).
    ///
    /// # Errors
    /// Whatever the model cannot compute; NaN is for values that are undefined, not for failures.
    fn compute(&self, t: Ts, universe: &[EntityId], x: &[Vec<f64>]) -> Result<Vec<Vec<f64>>>;
    /// Its versions, each with the time it became available (one model: none).
    fn vintages(&self) -> Vec<(Ts, u64)> {
        Vec::new()
    }
}

/// A time-series model: called once per entity per time, in time order.
pub trait TimeSeriesModel: Send + Sync {
    /// The declaration.
    fn spec(&self) -> ModelSpec;
    /// A fresh state for one entity.
    fn state(&self) -> Vec<f64>;
    /// One step: the current inputs (NaN when missing) and the entity's state. Returns one value
    /// per output (NaN while undefined, for example during warm-up).
    ///
    /// # Errors
    /// Whatever the model cannot compute; an error stops the evaluation.
    fn step(&self, t: Ts, x: &[f64], state: &mut Vec<f64>) -> Result<Vec<f64>>;
}

/// A model that computes whole series over the calendar at once: a book or a deal that steps its
/// own state. It must be causal by construction (a book's decisions see only what was known).
pub trait BatchModel: Send + Sync {
    /// The declaration: `reads` for its dependencies, `writes` for the series it produces.
    fn spec(&self) -> ModelSpec;
    /// The series it writes, in the order `writes` declares.
    ///
    /// # Errors
    /// Whatever it cannot compute; an error stops the evaluation.
    fn evaluate(&self, ws: &Workspace, cal: &Calendar) -> Result<Vec<Series>>;
    /// Whether it intervenes (spec, change 4). A deal whose manager decides other than as
    /// recorded is a counterfactual, and its outputs are not what happened.
    fn intervention(&self, _ws: &Workspace) -> Option<Intervention> {
        None
    }
    /// What it models, for the support matrix (spec, change 7).
    fn supports(&self) -> Vec<&'static str> {
        Vec::new()
    }
}

/// A model of any shape.
#[derive(Clone)]
pub enum Model {
    /// Cross-sectional.
    CrossSection(Arc<dyn CrossSectionModel>),
    /// Time series.
    TimeSeries(Arc<dyn TimeSeriesModel>),
    /// Whole series at once.
    Batch(Arc<dyn BatchModel>),
}

impl Model {
    /// Wraps a cross-sectional model.
    pub fn cross_section(m: impl CrossSectionModel + 'static) -> Self {
        Self::CrossSection(Arc::new(m))
    }

    /// Wraps a time-series model.
    pub fn time_series(m: impl TimeSeriesModel + 'static) -> Self {
        Self::TimeSeries(Arc::new(m))
    }

    /// Wraps a batch model.
    pub fn batch(m: impl BatchModel + 'static) -> Self {
        Self::Batch(Arc::new(m))
    }

    /// The declaration.
    #[must_use]
    pub fn spec(&self) -> ModelSpec {
        match self {
            Self::CrossSection(m) => m.spec(),
            Self::TimeSeries(m) => m.spec(),
            Self::Batch(m) => m.spec(),
        }
    }

    /// Its versions, each with the time it became available: (from, fingerprint).
    #[must_use]
    pub fn vintages(&self) -> Vec<(Ts, u64)> {
        match self {
            Self::CrossSection(m) => m.vintages(),
            _ => Vec::new(),
        }
    }

    /// Whether the model intervenes, rather than derives (spec, change 4). A stress is a
    /// scenario; a real-world model's outputs are a simulated world; a batch model says for
    /// itself. Signals and pricing models derive from their inputs.
    #[must_use]
    pub fn intervention(&self, ws: &Workspace) -> Option<Intervention> {
        if let Self::Batch(m) = self
            && let Some(i) = m.intervention(ws)
        {
            return Some(i);
        }
        let spec = self.spec();
        match spec.role {
            ModelRole::Stress => Some(Intervention::Scenario(spec.name)),
            ModelRole::RealWorld => Some(Intervention::Simulation(spec.name)),
            ModelRole::Signal | ModelRole::Pricing => None,
        }
    }

    /// The same computation with its declaration amended: a role, an availability date, the
    /// state it describes.
    #[must_use]
    pub fn amend(self, f: impl Fn(ModelSpec) -> ModelSpec + Send + Sync + 'static) -> Self {
        let f: Amend = Arc::new(f);
        match self {
            Self::CrossSection(m) => Self::CrossSection(Arc::new(AmendedCs(m, f))),
            Self::TimeSeries(m) => Self::TimeSeries(Arc::new(AmendedTs(m, f))),
            Self::Batch(m) => Self::Batch(Arc::new(AmendedBatch(m, f))),
        }
    }
}

type Amend = Arc<dyn Fn(ModelSpec) -> ModelSpec + Send + Sync>;

struct AmendedCs(Arc<dyn CrossSectionModel>, Amend);

impl CrossSectionModel for AmendedCs {
    fn spec(&self) -> ModelSpec {
        (self.1)(self.0.spec())
    }
    fn compute(&self, t: Ts, universe: &[EntityId], x: &[Vec<f64>]) -> Result<Vec<Vec<f64>>> {
        self.0.compute(t, universe, x)
    }
    fn vintages(&self) -> Vec<(Ts, u64)> {
        self.0.vintages()
    }
}

struct AmendedBatch(Arc<dyn BatchModel>, Amend);

impl BatchModel for AmendedBatch {
    fn spec(&self) -> ModelSpec {
        (self.1)(self.0.spec())
    }
    fn evaluate(&self, ws: &Workspace, cal: &Calendar) -> Result<Vec<Series>> {
        self.0.evaluate(ws, cal)
    }
    fn intervention(&self, ws: &Workspace) -> Option<Intervention> {
        self.0.intervention(ws)
    }
    fn supports(&self) -> Vec<&'static str> {
        self.0.supports()
    }
}

struct AmendedTs(Arc<dyn TimeSeriesModel>, Amend);

impl TimeSeriesModel for AmendedTs {
    fn spec(&self) -> ModelSpec {
        (self.1)(self.0.spec())
    }
    fn state(&self) -> Vec<f64> {
        self.0.state()
    }
    fn step(&self, t: Ts, x: &[f64], state: &mut Vec<f64>) -> Result<Vec<f64>> {
        self.0.step(t, x, state)
    }
}

/// Versions of one cross-sectional model, each used from when it became available: what an
/// as-known run could have used at each time. Before the first version there is no output. A
/// retrospective run uses the latest version throughout, and says so.
pub struct Vintaged {
    spec: ModelSpec,
    versions: Vec<(Ts, Arc<dyn CrossSectionModel>)>,
}

impl Vintaged {
    /// Versions of a model, with the times they became available.
    ///
    /// # Errors
    /// No versions, a time-series version, or versions whose inputs or outputs differ.
    pub fn new(name: &str, mut versions: Vec<(Ts, Model)>, retrospective: bool) -> Result<Self> {
        versions.sort_by_key(|(t, _)| *t);
        let first = versions.first().ok_or_else(|| Error::Invalid(format!("model {name} has no versions")))?.1.spec();
        let mut h = Hash64::new();
        let mut out = Vec::with_capacity(versions.len());
        for (t, m) in versions {
            let Model::CrossSection(m) = m else {
                return Err(Error::Invalid(format!("model {name}: vintages are for cross-sectional models")));
            };
            let s = m.spec();
            if s.inputs != first.inputs || s.outputs != first.outputs {
                return Err(Error::Invalid(format!("model {name}: every version needs the same inputs and outputs")));
            }
            h.i64(t.0);
            h.u64(s.fingerprint());
            out.push((t, m));
        }
        let parallel = out.iter().all(|(_, m)| m.spec().parallel);
        let spec = ModelSpec { name: name.to_owned(), params: h.finish(), available_from: Some(out[0].0), retrospective, parallel, ..first };
        Ok(Self { spec, versions: out })
    }
}

impl CrossSectionModel for Vintaged {
    fn spec(&self) -> ModelSpec {
        self.spec.clone()
    }

    fn vintages(&self) -> Vec<(Ts, u64)> {
        self.versions.iter().map(|(t, m)| (*t, m.spec().fingerprint())).collect()
    }

    fn compute(&self, t: Ts, universe: &[EntityId], x: &[Vec<f64>]) -> Result<Vec<Vec<f64>>> {
        let version = if self.spec.retrospective { self.versions.last() } else { self.versions.iter().rev().find(|(from, _)| *from <= t) };
        match version {
            Some((_, m)) => m.compute(t, universe, x),
            None => Ok(vec![vec![f64::NAN; universe.len()]; self.spec.outputs.len()]),
        }
    }
}
