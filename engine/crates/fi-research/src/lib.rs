//! The research driver.
//!
//! A [`Book`] is one or more policies (decision models with their clocks, scopes and priorities),
//! an execution model and ledger settings. A [`Strategy`] is the one-policy shortcut. Every book
//! runs on the same unit records, so a sweep of many variants computes the instruments once and
//! runs the variants in parallel. Research and Implement differ only in the execution model and
//! the decision times (invariant I4): this driver uses the kernel's ledger as is.

use std::sync::Arc;

use rayon::prelude::*;

pub mod manifest;

pub use manifest::{Manifest, SeriesEntry, check_requirements, support_matrix};

use fi_kernel::{
    Calendar, Contract, DecisionModel, Diagnostics, EntityId, Error, ExecRef, Execution, FillModel, Graph, GraphReport, Intervention, Ledger, LedgerConfig,
    LedgerOutput, MissingPolicy, Mode, Policy, Result, Ts, UnitRecords, Workspace, hash::Hash64, ledger,
};

/// A strategy: one policy over the whole book, filling through a fill model.
#[derive(Clone)]
pub struct Strategy {
    /// Name.
    pub name: String,
    /// What it decides at each rebalance.
    pub decide: Arc<dyn DecisionModel>,
    /// When it decides.
    pub schedule: Vec<Ts>,
    /// How orders fill.
    pub fill: Arc<dyn FillModel>,
    /// Ledger settings.
    pub config: LedgerConfig,
}

impl Strategy {
    /// A fingerprint of everything that changes the result besides the data.
    #[must_use]
    pub fn fingerprint(&self) -> u64 {
        Book::from(self.clone()).fingerprint()
    }
}

/// One policy of a book, owned.
#[derive(Clone)]
pub struct PolicySpec {
    /// Name.
    pub name: String,
    /// What it decides.
    pub decide: Arc<dyn DecisionModel>,
    /// When it decides.
    pub schedule: Vec<Ts>,
    /// The contracts it manages; none: the whole book.
    pub scope: Option<Vec<EntityId>>,
    /// Higher decides first.
    pub priority: i32,
    /// Steps from decision to submission.
    pub lag: usize,
    /// Steps an order works before expiring; none: the book's setting.
    pub time_in_force: Option<usize>,
}

/// How a book's orders execute.
#[derive(Clone)]
pub enum Executor {
    /// A fill model, order by order.
    Fill(Arc<dyn FillModel>),
    /// An execution process.
    Process(Arc<dyn Execution>),
}

impl Executor {
    fn as_ref(&self) -> ExecRef<'_> {
        match self {
            Self::Fill(f) => ExecRef::Fill(f.as_ref()),
            Self::Process(e) => ExecRef::Process(e.as_ref()),
        }
    }
}

/// A book: policies, an execution model and ledger settings.
#[derive(Clone)]
pub struct Book {
    /// Name.
    pub name: String,
    /// Its policies.
    pub policies: Vec<PolicySpec>,
    /// How its orders execute.
    pub execution: Executor,
    /// Ledger settings.
    pub config: LedgerConfig,
}

impl From<Strategy> for Book {
    fn from(s: Strategy) -> Self {
        let lag = s.config.execution_lag;
        Self {
            name: s.name.clone(),
            policies: vec![PolicySpec { name: s.name, decide: s.decide, schedule: s.schedule, scope: None, priority: 0, lag, time_in_force: None }],
            execution: Executor::Fill(s.fill),
            config: s.config,
        }
    }
}

impl Book {
    /// A fingerprint of everything that changes the result besides the data.
    #[must_use]
    pub fn fingerprint(&self) -> u64 {
        let mut h = Hash64::new();
        for p in &self.policies {
            h.str(&p.name);
            h.str(&p.decide.name());
            h.u64(p.decide.params());
            for t in &p.schedule {
                h.i64(t.0);
            }
            for e in p.scope.iter().flatten() {
                h.u64(u64::from(e.0));
            }
            h.i64(i64::from(p.priority));
            h.u64(p.lag as u64);
            h.u64(p.time_in_force.map_or(0, |t| t as u64 + 1));
        }
        h.str(&self.execution.as_ref().name());
        h.u64(self.config.fingerprint());
        h.finish()
    }

    /// The book's ledger on `records`.
    ///
    /// # Errors
    /// See [`Ledger::build`].
    pub fn ledger<'a>(&'a self, ws: &'a Workspace, records: &'a UnitRecords) -> Result<Ledger<'a>> {
        let policies = self
            .policies
            .iter()
            .map(|p| Policy {
                name: p.name.clone(),
                decide: p.decide.as_ref(),
                schedule: p.schedule.clone(),
                scope: p.scope.clone(),
                priority: p.priority,
                lag: p.lag,
                time_in_force: p.time_in_force,
            })
            .collect();
        Ledger::build(ws, records, policies, self.execution.as_ref(), self.config.clone())
    }
}

/// What a research session prepares once and every strategy shares.
pub struct Prepared {
    /// The instruments' unit records.
    pub records: UnitRecords,
    /// What the instruments did not observe.
    pub diagnostics: Diagnostics,
    /// What the model graph evaluated or found cached.
    pub graph: GraphReport,
}

/// Evaluates the models, then steps the contracts.
///
/// # Errors
/// From the graph or the unit records.
pub fn prepare(ws: &mut Workspace, graph: &Graph, contracts: &[Arc<dyn Contract>], calendar: &Calendar, missing: MissingPolicy) -> Result<Prepared> {
    let report = graph.evaluate(ws, calendar)?;
    let (records, diagnostics) = UnitRecords::compute(contracts, ws, calendar, missing)?;
    Ok(Prepared { records, diagnostics, graph: report })
}

/// Runs one strategy.
///
/// # Errors
/// From the ledger.
pub fn run(ws: &Workspace, records: &UnitRecords, strategy: &Strategy) -> Result<LedgerOutput> {
    ledger::run(ws, records, strategy.decide.as_ref(), &strategy.schedule, strategy.fill.as_ref(), strategy.config.clone())
}

/// Runs many strategies in parallel on shared records. Results come back in input order.
#[must_use]
pub fn sweep(ws: &Workspace, records: &UnitRecords, strategies: &[Strategy]) -> Vec<Result<LedgerOutput>> {
    strategies.par_iter().map(|s| run(ws, records, s)).collect()
}

/// Runs one book.
///
/// # Errors
/// From the ledger.
pub fn run_book(ws: &Workspace, records: &UnitRecords, book: &Book) -> Result<LedgerOutput> {
    book.ledger(ws, records)?.run()
}

/// Runs many books in parallel on shared records. Results come back in input order.
#[must_use]
pub fn sweep_books(ws: &Workspace, records: &UnitRecords, books: &[Book]) -> Vec<Result<LedgerOutput>> {
    let reporter = fi_kernel::Reporter::start("fi.sweep", books.len() as u64, "books");
    let done = std::sync::atomic::AtomicU64::new(0);
    let out = books
        .par_iter()
        .map(|b| {
            let r = b.ledger(ws, records).and_then(|l| l.quiet().run());
            reporter.report(done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1, None);
            r
        })
        .collect();
    reporter.finish(true);
    out
}

/// Runs several books in lockstep through one shared execution model: they compete for its
/// capacity (accounts that must be implementable together).
///
/// # Errors
/// From any book's ledger.
pub fn run_together(ws: &Workspace, records: &UnitRecords, books: &[Book], shared: &dyn Execution) -> Result<Vec<LedgerOutput>> {
    let ledgers = books.iter().map(|b| b.ledger(ws, records)).collect::<Result<Vec<_>>>()?;
    ledger::run_together(ledgers, shared)
}

/// What a book's run is (spec, change 4), derived from its decisions and the world its contracts
/// read rather than declared.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum RunMode {
    /// Every decision replays observed holdings or trades, in the observed world.
    HoldingsReplay,
    /// The book's decisions are computed; the world is observed.
    PolicyBacktest,
    /// The world holds values that did not happen: a stress, a scenario series.
    Scenario,
    /// The world holds a decision other than the one taken (a deal's alternative manager).
    Counterfactual,
    /// The world is generated by stochastic models.
    Simulation,
}

impl std::fmt::Display for RunMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::HoldingsReplay => "holdings replay",
            Self::PolicyBacktest => "policy backtest",
            Self::Scenario => "scenario",
            Self::Counterfactual => "counterfactual",
            Self::Simulation => "simulation",
        })
    }
}

/// The run mode of `book` on contracts that read `reads`: the strongest intervention in the
/// world they read, else a policy backtest if any policy decides other than as recorded, else a
/// holdings replay.
#[must_use]
pub fn run_mode(ws: &Workspace, reads: &[String], book: &Book) -> RunMode {
    let world = reads.iter().filter_map(|r| ws.series(r).ok()).filter_map(|s| s.provenance().intervention.clone()).map(|i| Mode::of(&i)).max();
    match world {
        Some(Mode::Simulation) => RunMode::Simulation,
        Some(Mode::Counterfactual) => RunMode::Counterfactual,
        Some(_) => RunMode::Scenario,
        None if book.policies.iter().any(|p| p.decide.intervenes(ws)) => RunMode::PolicyBacktest,
        None => RunMode::HoldingsReplay,
    }
}

/// The interventions in the world `reads` names: what makes a run other than history.
#[must_use]
pub fn interventions(ws: &Workspace, reads: &[String]) -> Vec<(String, Intervention)> {
    reads.iter().filter_map(|r| ws.series(r).ok().and_then(|s| s.provenance().intervention.clone().map(|i| (r.clone(), i)))).collect()
}

/// A run's NAV against an official one (a fund's reported NAV, an administrator's figures).
/// Differences stay visible: nothing is plugged.
#[derive(Clone, Debug, PartialEq)]
pub struct Reconciliation {
    /// The run's times.
    pub times: Vec<Ts>,
    /// The run's NAV.
    pub engine: Vec<f64>,
    /// The official NAV at each time (NaN where none is reported).
    pub official: Vec<f64>,
    /// Engine less official: what the engine does not explain.
    pub difference: Vec<f64>,
}

impl Reconciliation {
    /// The largest absolute difference, where both are known.
    #[must_use]
    pub fn worst(&self) -> f64 {
        self.difference.iter().filter(|d| d.is_finite()).fold(0.0, |m, d| m.max(d.abs()))
    }
}

/// Reconciles `out`'s NAV with `series.column` for `entity` (`EntityId::NONE`: a market-wide
/// series), reported for each time, in its latest vintage.
///
/// # Errors
/// An unknown series or column.
pub fn reconcile(ws: &Workspace, out: &LedgerOutput, series: &str, column: &str, entity: EntityId) -> Result<Reconciliation> {
    let s = ws.outcome(series)?;
    let c = s.column(column)?;
    if out.times.is_empty() {
        return Err(Error::Invalid("the run has no times to reconcile".into()));
    }
    let official: Vec<f64> = out.times.iter().map(|t| s.exact(c, entity, *t, Ts::MAX).map_or(f64::NAN, |h| h.value)).collect();
    let difference = out.nav.iter().zip(&official).map(|(e, o)| e - o).collect();
    Ok(Reconciliation { times: out.times.clone(), engine: out.nav.clone(), official, difference })
}

/// Runs `each` in `n` worlds, in parallel (spec, change 6). World `w` is a copy of `base` that
/// shares every series with it by reference, in which the models `world(w)` returns are
/// evaluated over `cal`. Evaluate whatever does not vary by world in `base` beforehand: it is
/// then computed once and shared. Draws keyed by world make each world reproducible on its own,
/// whatever the threads. Results come back in world order; return summaries, not whole runs, to
/// keep memory flat.
///
/// # Errors
/// The first world's error, from its models or `each`.
pub fn worlds<R: Send>(
    base: &Workspace,
    n: u64,
    cal: &Calendar,
    world: impl Fn(u64) -> Graph + Sync,
    each: impl Fn(u64, &Workspace, &GraphReport) -> Result<R> + Sync,
) -> Result<Vec<R>> {
    (0..n)
        .into_par_iter()
        .map(|w| {
            let mut ws = base.clone();
            let report = world(w).evaluate(&mut ws, cal)?;
            each(w, &ws, &report)
        })
        .collect()
}

/// A distribution across worlds (NaN values are left out and counted).
#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    /// Finite values.
    pub n: usize,
    /// Values that were not numbers.
    pub missing: usize,
    /// Mean.
    pub mean: f64,
    /// Standard deviation.
    pub sd: f64,
    /// Smallest.
    pub min: f64,
    /// 5th percentile.
    pub p05: f64,
    /// Median.
    pub p50: f64,
    /// 95th percentile.
    pub p95: f64,
    /// Largest.
    pub max: f64,
}

impl Summary {
    /// The summary of `values`; percentiles interpolate linearly.
    #[must_use]
    pub fn of(values: &[f64]) -> Self {
        let mut v: Vec<f64> = values.iter().copied().filter(|x| x.is_finite()).collect();
        v.sort_by(f64::total_cmp);
        let n = v.len();
        let q = |p: f64| -> f64 {
            if n == 0 {
                return f64::NAN;
            }
            let x = p * (n - 1) as f64;
            let (lo, hi) = (x.floor() as usize, x.ceil() as usize);
            v[lo] + (v[hi] - v[lo]) * (x - lo as f64)
        };
        let mean = if n == 0 { f64::NAN } else { v.iter().sum::<f64>() / n as f64 };
        let sd = if n < 2 { f64::NAN } else { (v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1) as f64).sqrt() };
        Self {
            n,
            missing: values.len() - n,
            mean,
            sd,
            min: v.first().copied().unwrap_or(f64::NAN),
            p05: q(0.05),
            p50: q(0.5),
            p95: q(0.95),
            max: v.last().copied().unwrap_or(f64::NAN),
        }
    }
}
