//! The ledger: a book's holdings, accounts, orders and trades over the records' calendar.
//!
//! Balances change only through balanced transfers (axiom 5, spec change 1). Decisions produce
//! intentions; execution, settlement and capital flows produce transfers. Each step:
//!
//! 1. books contract events ([`flows`]): the held units' terms and cash flows, FX translation, a
//!    coupon gone ex as a receivable, then interest on cash and financing, and borrow fees; then
//!    the records' holding transitions ([`transitions`]);
//! 2. runs the book's phases in its declared order ([`StepOrder`]): weight resets, capital flows
//!    ([`capital`]), decisions and orders ([`decisions`]), execution ([`fills`]), settlement
//!    ([`settlement`]), recalls and agreements ([`financing`]), distribution ([`distribution`]),
//!    and the cash guard and constraints ([`guards`]);
//! 3. marks: NAV in the base currency, checked against the identity
//!    `ΔNAV = income + price + principal + other − financing + currency − costs + capital`.
//!
//! Events sharing a time resolve in the declared order, which goes into the run's settings and so
//! its manifest; a book can declare another to test whether an ordering assumption is material.
//! Cash is exact (fixed point) in every currency. A breach of the identity fails the run. The
//! whole changing state is one value ([`LedgerState`]): a checkpoint is a copy of it, and a run
//! resumed from a checkpoint reaches the same state as a full run.

mod capital;
mod config;
mod decisions;
mod distribution;
mod execution;
mod fills;
mod financing;
mod flows;
mod guards;
mod journal;
mod order;
mod settlement;
mod transitions;

pub use config::{
    Advance, Agreement, AgreementKind, Basis, Breach, CapitalFlows, CashPolicy, CashRate, Collected, Constraint, ConstraintScope, Distribution, Distributor,
    DistributorRef, DistributorState, FlowCtx, FlowModel, FxConversion, Horizon, LedgerConfig, OpeningDue, Response, Routing, Settlement, ShortPolicy,
};
pub use execution::{EachOrder, Execution, OrderRecord, OrderStats, OrderStatus, WorkingOrder};
pub use journal::{Cause, Entry, EntryKind};
pub use order::{Phase, StepOrder};

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use crate::{
    Error, Result,
    cash::Cash,
    entity::EntityId,
    fill::{Fill, FillModel, Order},
    hash::Hash64,
    measure::Measure,
    policy::HoldingPolicy,
    progress::Reporter,
    records::UnitRecords,
    series::Series,
    time::Ts,
    view::View,
    workspace::Workspace,
};

/// What a decision returns: intentions, never balances.
#[derive(Clone)]
pub enum Targets {
    /// Market-value weights of NAV.
    Weights(Vec<(EntityId, f64)>),
    /// Quantities in units.
    Quantities(Vec<(EntityId, f64)>),
    /// Targets in any measure: quantity = target ÷ measure per unit.
    InMeasure {
        /// The measure.
        measure: Arc<dyn Measure>,
        /// Target exposure per entity.
        targets: Vec<(EntityId, f64)>,
    },
    /// Orders: units to buy (positive) or sell, on top of the book. Nothing else is sold.
    Orders(Vec<(EntityId, f64)>),
}

/// What a decision model sees: data known by `t`, the book, its cash and its working orders.
pub struct DecisionCtx<'a> {
    /// Decision time.
    pub t: Ts,
    /// Calendar index.
    pub k: usize,
    /// Series and model outputs known by `t`.
    pub view: View<'a>,
    /// Unit records (read history through [`UnitRecords::returns_known`]).
    pub records: &'a UnitRecords,
    /// Current holdings: (record row, quantity).
    pub holdings: &'a [(usize, f64)],
    /// NAV before trading, in the base currency.
    pub nav: f64,
    /// Orders still working, from every policy.
    pub orders: &'a [WorkingOrder],
    /// Cash available to spend in each currency (settled cash less committed payables).
    pub cash: &'a [(EntityId, f64)],
    /// The deciding policy.
    pub policy: usize,
    /// Notes for the journal: an optimiser's status, a binding constraint.
    pub notes: &'a Mutex<Vec<String>>,
    /// The book's own state, as its distributor last published it (a deal's `"reinvesting"`).
    pub state: &'a [(String, f64)],
}

impl DecisionCtx<'_> {
    /// A value of the book's own state, if published.
    #[must_use]
    pub fn state(&self, key: &str) -> Option<f64> {
        self.state.iter().find(|(k, _)| k == key).map(|(_, v)| *v)
    }

    /// Records a note in the journal.
    pub fn note(&self, note: impl Into<String>) {
        if let Ok(mut n) = self.notes.lock() {
            n.push(note.into());
        }
    }
}

/// A decision model: the book and data in, intentions out.
pub trait DecisionModel: Send + Sync {
    /// Name.
    fn name(&self) -> String;
    /// Hash of its parameters.
    fn params(&self) -> u64;
    /// Targets at a decision time, or `None` to keep the book.
    ///
    /// # Errors
    /// Whatever the model cannot decide (a missing series, a failed optimiser): the run stops.
    fn decide(&self, ctx: &DecisionCtx<'_>) -> Result<Option<Targets>>;
    /// Whether its decisions differ from what was recorded (spec, change 4). A computed decision
    /// does; a replay of observed holdings or trades does not.
    fn intervenes(&self, _ws: &Workspace) -> bool {
        true
    }
}

/// One policy of a book: a decision model, its clock, the contracts it manages and its priority.
#[derive(Clone)]
pub struct Policy<'a> {
    /// Name.
    pub name: String,
    /// What it decides.
    pub decide: &'a dyn DecisionModel,
    /// When it decides.
    pub schedule: Vec<Ts>,
    /// The contracts it manages; none: the whole book. Held contracts in scope that its targets
    /// omit are sold; contracts outside it are never touched.
    pub scope: Option<Vec<EntityId>>,
    /// Higher decides first. A lower policy cannot trade a contract that a higher one is trading
    /// in the same step (noted).
    pub priority: i32,
    /// Steps from decision to submission.
    pub lag: usize,
    /// Steps an order works before expiring; none: the book's setting.
    pub time_in_force: Option<usize>,
}

impl<'a> Policy<'a> {
    /// A policy over the whole book.
    #[must_use]
    pub fn new(name: &str, decide: &'a dyn DecisionModel, schedule: &[Ts], lag: usize) -> Self {
        Self { name: name.to_owned(), decide, schedule: schedule.to_vec(), scope: None, priority: 0, lag, time_in_force: None }
    }
}

/// How a book's orders execute: a fill model per order, or an execution process.
#[derive(Clone, Copy)]
pub enum ExecRef<'a> {
    /// Each order fills on its own, at once or not at all.
    Fill(&'a dyn FillModel),
    /// An execution process.
    Process(&'a dyn Execution),
}

impl ExecRef<'_> {
    fn latency(self) -> usize {
        match self {
            Self::Fill(_) => 0,
            Self::Process(e) => e.latency(),
        }
    }

    /// The model's name.
    #[must_use]
    pub fn name(self) -> String {
        match self {
            Self::Fill(f) => f.name(),
            Self::Process(e) => e.name(),
        }
    }

    fn work(self, t: Ts, orders: &[WorkingOrder], values: &[f64]) -> Vec<Fill> {
        match self {
            Self::Fill(f) => orders.iter().zip(values).map(|(o, v)| f.fill(&Order { entity: o.entity, qty: o.remaining, t }, *v)).collect(),
            Self::Process(e) => e.work(t, orders, values),
        }
    }
}

/// A constraint's breaches over a run.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BreachRecord {
    /// The constraint.
    pub constraint: String,
    /// Whether it is hard.
    pub hard: bool,
    /// Times it was breached.
    pub count: u64,
    /// The first breach.
    pub first: Option<Ts>,
    /// The largest excess.
    pub worst: f64,
    /// Σ penalty × excess, for a soft rule.
    pub penalty: f64,
}

/// The accounts: one value per calendar time, and the run's totals.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LedgerOutput {
    /// Calendar times.
    pub times: Vec<Ts>,
    /// Net asset value, in the base currency.
    pub nav: Vec<f64>,
    /// Cash in every currency, in the base currency.
    pub cash: Vec<f64>,
    /// Income term.
    pub income: Vec<f64>,
    /// Price term.
    pub price: Vec<f64>,
    /// Principal term.
    pub principal: Vec<f64>,
    /// Other flows term (fees, protection, recovery, exchange).
    pub other: Vec<f64>,
    /// Financing term (a cost): funding legs, borrowing, borrow fees.
    pub financing: Vec<f64>,
    /// Trading costs.
    pub costs: Vec<f64>,
    /// Turnover, as a fraction of NAV.
    pub turnover: Vec<f64>,
    /// Number of holdings.
    pub holdings: Vec<usize>,
    /// FX translation of everything held in other currencies.
    pub currency: Vec<f64>,
    /// External capital: subscriptions less redemptions.
    pub capital: Vec<f64>,
    /// Receivables less payables not yet settled, in the base currency.
    pub unsettled: Vec<f64>,
    /// NAV per unit (100 at the start): returns are time-weighted through capital flows.
    pub nav_per_unit: Vec<f64>,
    /// Holdings at the end: the economic positions.
    pub final_holdings: Vec<(EntityId, f64)>,
    /// Holdings at the end as legally owned: units bought and not yet delivered left out, units
    /// sold and not yet delivered still in.
    pub final_settled: Vec<(EntityId, f64)>,
    /// Receivables at the end, not yet settled, in the base currency.
    pub final_receivable: f64,
    /// Payables at the end, not yet settled, in the base currency.
    pub final_payable: f64,
    /// Cash at the end, by currency.
    pub final_cash: Vec<(EntityId, f64)>,
    /// Largest identity error seen, as a fraction of NAV.
    pub max_identity_error: f64,
    /// Fills that moved a quantity.
    pub trades: usize,
    /// Trades not made because the unit had no value or no measure at the time (I8: counted).
    pub skipped_trades: u64,
    /// Orders that ended without filling in full (I8: counted).
    pub unfilled_orders: u64,
    /// Intentions and orders still pending when the calendar ended (I8: counted, not dropped).
    pub pending_at_end: u64,
    /// Buys cut or refused by the cash guard.
    pub cash_limited: u64,
    /// Sales cut by the short guard.
    pub short_limited: u64,
    /// The orders' totals.
    pub orders: OrderStats,
    /// Each constraint's breaches.
    pub breaches: Vec<BreachRecord>,
    /// When the book failed economically (an obligation it could not pay, or NAV at or below
    /// zero). A failed path stays in every statistic.
    pub failed_at: Option<Ts>,
    /// The largest obligation the cash could not meet, in the base currency.
    pub shortfall: f64,
    /// NAV after the horizon liquidation, where the run liquidates.
    pub liquidation_nav: Option<f64>,
    /// Marked value of what the liquidation could not sell.
    pub liquidation_residual: f64,
    /// Every transfer with its cause (when the journal is kept).
    pub journal: Vec<Entry>,
    /// Every order's life (when the journal is kept).
    pub order_log: Vec<OrderRecord>,
    /// Holdings at every time (when kept).
    pub holdings_path: Vec<Vec<(EntityId, f64)>>,
    /// Each financing agreement's path.
    pub agreements: Vec<AgreementPath>,
    /// Obligations due and still unpaid at each time (payables and margin calls), in the base
    /// currency: what the book failed to pay, still owed.
    pub overdue: Vec<f64>,
}

/// A financing agreement's state at every time: what is owed, what the collateral supports, and
/// what the collateral is worth stay separate (F20).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgreementPath {
    /// The agreement.
    pub name: String,
    /// The balance borrowed (secured), or cash collateral posted (a CSA; negative: received).
    pub balance: Vec<f64>,
    /// What the pledged collateral supports (secured), or the netting set's value (a CSA).
    pub capacity: Vec<f64>,
    /// A margin call outstanding, after this time's payment.
    pub call: Vec<f64>,
    /// The call the lender made at this time, if it assessed margin.
    pub called: Vec<f64>,
    /// What the book paid against calls at this time.
    pub paid: Vec<f64>,
    /// The pledged collateral's value (secured), or the collateral required (a CSA).
    pub value: Vec<f64>,
}

impl LedgerOutput {
    fn prices(&self) -> &[f64] {
        if self.nav_per_unit.len() == self.nav.len() { &self.nav_per_unit } else { &self.nav }
    }

    /// Time-weighted returns per period (the first is zero). After an economic failure, NaN:
    /// returns are not chained through insolvency.
    #[must_use]
    pub fn returns(&self) -> Vec<f64> {
        let p = self.prices();
        if p.is_empty() {
            return Vec::new();
        }
        let failed = self.failed_at.and_then(|t| self.times.iter().position(|x| *x == t));
        std::iter::once(0.0)
            .chain(p.windows(2).enumerate().map(|(j, w)| if failed.is_some_and(|f| j + 1 > f) { f64::NAN } else { w[1] / w[0] - 1.0 }))
            .collect()
    }

    /// Total time-weighted return over the run.
    #[must_use]
    pub fn total_return(&self) -> f64 {
        let p = self.prices();
        match (p.first(), p.last()) {
            (Some(a), Some(b)) if *a != 0.0 => b / a - 1.0,
            _ => 0.0,
        }
    }

    /// Content hash of the accounts.
    #[must_use]
    pub fn version(&self) -> u64 {
        let mut h = Hash64::new();
        for t in &self.times {
            h.i64(t.0);
        }
        for arr in [&self.nav, &self.cash, &self.income, &self.price, &self.principal, &self.other, &self.financing, &self.costs, &self.currency, &self.capital]
        {
            for v in arr.iter() {
                h.f64(*v);
            }
        }
        for (e, q) in &self.final_holdings {
            h.u64(u64::from(e.0));
            h.f64(*q);
        }
        h.finish()
    }
}

#[derive(Clone, Debug)]
struct Due {
    id: u64,
    k: usize,
    slot: usize,
    amount: Cash,
    /// The part of a receipt that is interest: where the book routes collections, it goes to the
    /// interest account when it is received.
    income: Cash,
}

/// An intention waiting for its submission step.
#[derive(Clone)]
struct Queued {
    due: usize,
    policy: usize,
    targets: Targets,
}

/// Everything about a book that changes as it steps: what a checkpoint holds.
#[derive(Clone)]
pub struct LedgerState {
    k: usize,
    q: Vec<f64>,
    held: Vec<usize>,
    cash: Vec<Cash>,
    interest_collections: Vec<Cash>,
    dues: Vec<Due>,
    deliveries: Vec<(usize, f64, usize)>,
    working: Vec<WorkingOrder>,
    queued: VecDeque<Queued>,
    weights: Vec<(usize, f64)>,
    units: f64,
    fx: Vec<f64>,
    seq: u64,
    collected: Collected,
    dist: Option<Box<dyn DistributorState>>,
    published: Vec<(String, f64)>,
    agreements: Vec<AgreementState>,
    /// Units of each row pledged this step, and to which agreement.
    pledged: Vec<(f64, usize)>,
    out: LedgerOutput,
}

#[derive(Clone, Debug, Default)]
struct AgreementState {
    balance: f64,
    call: f64,
    call_due: usize,
    capacity: f64,
    value: f64,
    called: f64,
    paid: f64,
    matured: bool,
}

/// An agreement's terms, bound to the records and series.
struct AgreementRt<'a> {
    rows: Vec<(usize, Col<'a>, f64)>,
    set: Vec<usize>,
    rate: Col<'a>,
}

impl Clone for Box<dyn DistributorState> {
    fn clone(&self) -> Self {
        (**self).clone_state()
    }
}

impl LedgerState {
    /// The step the state is at.
    #[must_use]
    pub fn step(&self) -> usize {
        self.k
    }

    /// The accounts so far.
    #[must_use]
    pub fn output(&self) -> &LedgerOutput {
        &self.out
    }

    /// Cash in each currency slot (the base first).
    #[must_use]
    pub fn cash(&self) -> Vec<f64> {
        self.cash.iter().map(|c| c.to_f64()).collect()
    }

    /// The interest collection account in each currency slot ([`Routing::Collections`]).
    #[must_use]
    pub fn interest_collections(&self) -> Vec<f64> {
        self.interest_collections.iter().map(|c| c.to_f64()).collect()
    }

    /// The distributor's state.
    #[must_use]
    pub fn distributor_state(&self) -> Option<&dyn DistributorState> {
        self.dist.as_deref()
    }

    /// The book's own state, as its distributor last published it.
    #[must_use]
    pub fn published(&self) -> &[(String, f64)] {
        &self.published
    }
}

struct PolicyRt<'a> {
    decide: &'a dyn DecisionModel,
    schedule: Vec<Ts>,
    scope: Option<Vec<bool>>,
    priority: i32,
    lag: usize,
    tif: usize,
}

struct Acc {
    income: f64,
    price: f64,
    principal: f64,
    other: f64,
    financing: f64,
    currency: f64,
    costs: f64,
    capital: f64,
    turnover: f64,
}

impl Acc {
    fn zero() -> Self {
        Self { income: 0.0, price: 0.0, principal: 0.0, other: 0.0, financing: 0.0, currency: 0.0, costs: 0.0, capital: 0.0, turnover: 0.0 }
    }
}

/// A series column bound once.
type Col<'a> = Option<(&'a Series, usize)>;

/// The ledger, stepping a book through the records' calendar.
pub struct Ledger<'a> {
    ws: &'a Workspace,
    records: &'a UnitRecords,
    policies: Vec<PolicyRt<'a>>,
    order: Vec<usize>,
    exec: ExecRef<'a>,
    cfg: LedgerConfig,
    slots: Vec<EntityId>,
    row_slot: Vec<usize>,
    settle_lag: Vec<usize>,
    fx_series: Col<'a>,
    /// Each currency slot's deposit rate and its day count.
    deposit: Vec<(usize, Col<'a>, fi_terms::DayCount)>,
    borrow_rate: Col<'a>,
    available: Col<'a>,
    flows: Col<'a>,
    groups: Vec<Col<'a>>,
    fin: Vec<AgreementRt<'a>>,
    notes: Mutex<Vec<String>>,
    /// The declared phases before execution and after it.
    before: Vec<Phase>,
    after: Vec<Phase>,
    /// The step each of the records' transitions applies at.
    transition_k: Vec<usize>,
    /// Whether `run` reports its progress (not inside a sweep, which reports its own).
    quiet: bool,
    st: LedgerState,
}

/// What a step carries from phase to phase.
struct Stepping {
    k: usize,
    t: Ts,
    acc: Acc,
    nav_prev: f64,
    /// The NAV decisions see: measured before the step's capital flows, wherever they run.
    nav_pre: Option<f64>,
}

fn bind<'a>(ws: &'a Workspace, p: Option<&(String, String)>, outcome: bool) -> Result<Col<'a>> {
    match p {
        None => Ok(None),
        Some((s, c)) => {
            let s = if outcome { ws.outcome(s)? } else { ws.series(s)? };
            Ok(Some((s.as_ref(), s.column(c)?)))
        }
    }
}

fn read(col: Col<'_>, e: EntityId, t: Ts, known_by: Ts) -> Option<f64> {
    col.and_then(|(s, c)| s.asof(c, e, t, known_by)).map(|h| h.value)
}

const TINY: f64 = 1e-9;

impl<'a> Ledger<'a> {
    /// A one-policy ledger whose orders fill through a fill model.
    ///
    /// # Errors
    /// See [`Ledger::build`].
    pub fn new(
        ws: &'a Workspace,
        records: &'a UnitRecords,
        decide: &'a dyn DecisionModel,
        schedule: &[Ts],
        fill: &'a dyn FillModel,
        cfg: LedgerConfig,
    ) -> Result<Self> {
        let lag = cfg.execution_lag;
        Self::build(ws, records, vec![Policy::new("policy", decide, schedule, lag)], ExecRef::Fill(fill), cfg)
    }

    /// A ledger with any number of policies and an execution model.
    ///
    /// # Errors
    /// An inconsistent constraint, a policy scope naming an unknown contract, contracts in several
    /// currencies without FX rates, or a series the settings name that does not exist.
    pub fn build(ws: &'a Workspace, records: &'a UnitRecords, policies: Vec<Policy<'a>>, exec: ExecRef<'a>, cfg: LedgerConfig) -> Result<Self> {
        for c in &cfg.constraints {
            c.validate()?;
        }
        if cfg.time_in_force == 0 {
            return Err(Error::Invalid("an order must work for at least one step (time in force ≥ 1)".into()));
        }
        let base = cfg.base_currency;
        let mut slots = vec![base];
        let row_slot: Vec<usize> = records
            .currency
            .iter()
            .map(|c| {
                let c = if *c == EntityId::NONE { base } else { *c };
                slots.iter().position(|s| *s == c).unwrap_or_else(|| {
                    slots.push(c);
                    slots.len() - 1
                })
            })
            .collect();
        if slots.len() > 1 && cfg.fx.is_none() {
            let names: Vec<&str> = slots.iter().map(|s| if *s == EntityId::NONE { "the base currency" } else { ws.entities.name(*s) }).collect();
            return Err(Error::Invalid(format!("contracts in several currencies ({names:?}) need FX rates")));
        }
        let lag_of: HashMap<EntityId, usize> = cfg.settlement.by_contract.iter().copied().collect();
        let settle_lag = records.entities().iter().map(|e| lag_of.get(e).copied().unwrap_or(cfg.settlement.lag)).collect();
        let fx_series = bind(ws, cfg.fx.as_ref(), true)?;
        let mut deposit = Vec::with_capacity(cfg.deposit.len());
        for d in &cfg.deposit {
            let c = if d.currency == EntityId::NONE { base } else { d.currency };
            let slot = slots
                .iter()
                .position(|s| *s == c)
                .ok_or_else(|| Error::Invalid(format!("a deposit rate for {}, a currency no contract of the book is in", ws.entities.name(d.currency))))?;
            if d.day_count.needs_reference() {
                return Err(Error::Invalid(format!("cash accrues by a day count without coupon periods, not {}", d.day_count)));
            }
            if deposit.iter().any(|(s, _, _)| *s == slot) {
                return Err(Error::Invalid(format!("two deposit rates for {}", ws.entities.name(c))));
            }
            deposit.push((slot, bind(ws, Some(&d.rate), true)?, d.day_count));
        }
        if cfg.day_count.needs_reference() {
            return Err(Error::Invalid(format!("cash accrues by a day count without coupon periods, not {}", cfg.day_count)));
        }
        let borrow_rate = match &cfg.cash {
            CashPolicy::Borrow { rate, .. } => bind(ws, rate.as_ref(), true)?,
            _ => None,
        };
        let available = match &cfg.shorts {
            ShortPolicy::Borrow { available, .. } => bind(ws, available.as_ref(), false)?,
            _ => None,
        };
        let flows = match &cfg.flows {
            Some(CapitalFlows::Series { series, column, .. }) => bind(ws, Some(&(series.clone(), column.clone())), false)?,
            _ => None,
        };
        let groups = cfg
            .constraints
            .iter()
            .map(|c| match &c.scope {
                ConstraintScope::PerGroup { series, column } => bind(ws, Some(&(series.clone(), column.clone())), false),
                _ => Ok(None),
            })
            .collect::<Result<Vec<_>>>()?;
        let n = records.entities().len();
        let row_of = |e: &EntityId, what: &str| {
            records.idx(*e).ok_or_else(|| Error::Unknown(format!("agreement {what} names {}, which has no contract", ws.entities.name(*e))))
        };
        let mut fin = Vec::with_capacity(cfg.financing.len());
        for a in &cfg.financing {
            match &a.kind {
                AgreementKind::Secured { collateral, rate, limit, .. } => {
                    if limit.is_nan() || *limit < 0.0 {
                        return Err(Error::Invalid(format!("agreement {}: the limit is {limit}", a.name)));
                    }
                    let mut rows = Vec::with_capacity(collateral.len());
                    for (e, adv) in collateral {
                        let i = row_of(e, &a.name)?;
                        rows.push(match adv {
                            Advance::Fixed(x) => (i, None, *x),
                            Advance::Series(s, c) => (i, bind(ws, Some(&(s.clone(), c.clone())), true)?, f64::NAN),
                        });
                    }
                    fin.push(AgreementRt { rows, set: Vec::new(), rate: bind(ws, rate.as_ref(), true)? });
                }
                AgreementKind::Csa { set, every, .. } => {
                    if *every == 0 {
                        return Err(Error::Invalid(format!("agreement {}: margin every 0 steps", a.name)));
                    }
                    let set = set.iter().map(|e| row_of(e, &a.name)).collect::<Result<_>>()?;
                    fin.push(AgreementRt { rows: Vec::new(), set, rate: None });
                }
            }
        }
        let agreements = cfg
            .financing
            .iter()
            .map(|a| AgreementState { balance: if let AgreementKind::Secured { opening, .. } = a.kind { opening } else { 0.0 }, ..AgreementState::default() })
            .collect();
        let mut rts = Vec::with_capacity(policies.len());
        for p in &policies {
            let mut schedule = p.schedule.clone();
            schedule.sort_unstable();
            schedule.dedup();
            let scope = match &p.scope {
                None => None,
                Some(es) => {
                    let mut mask = vec![false; n];
                    for e in es {
                        let i = records
                            .idx(*e)
                            .ok_or_else(|| Error::Unknown(format!("policy {} manages {}, which has no contract", p.name, ws.entities.name(*e))))?;
                        mask[i] = true;
                    }
                    Some(mask)
                }
            };
            rts.push(PolicyRt {
                decide: p.decide,
                schedule,
                scope,
                priority: p.priority,
                lag: p.lag,
                tif: p.time_in_force.unwrap_or(cfg.time_in_force).max(1),
            });
        }
        let mut order: Vec<usize> = (0..rts.len()).collect();
        order.sort_by_key(|&i| (std::cmp::Reverse(rts[i].priority), i));
        let nslots = slots.len();
        let mut cash = vec![Cash::from_f64(0.0); nslots];
        cash[0] = Cash::from_f64(cfg.initial_cash);
        let breaches = cfg.constraints.iter().map(|c| BreachRecord { constraint: c.name.clone(), hard: c.hard, ..BreachRecord::default() }).collect();
        let mut q = vec![0.0; n];
        for (e, qty) in &cfg.opening {
            let i = records.idx(*e).ok_or_else(|| Error::Unknown(format!("an opening position in {}, which has no contract", ws.entities.name(*e))))?;
            if !qty.is_finite() {
                return Err(Error::Invalid(format!("the opening position in {} is not a number", ws.entities.name(*e))));
            }
            q[i] += qty;
        }
        let held: Vec<usize> = (0..n).filter(|&i| q[i] != 0.0).collect();
        // Receivables and payables in flight at the first time settle at their own instants.
        let mut seq = 0;
        let mut dues = Vec::with_capacity(cfg.opening_dues.len());
        for d in &cfg.opening_dues {
            let c = if d.currency == EntityId::NONE { base } else { d.currency };
            let slot = slots.iter().position(|s| *s == c).ok_or_else(|| {
                Error::Invalid(format!("an opening receivable or payable in {}, a currency none of the book's contracts is in", ws.entities.name(c)))
            })?;
            if !d.amount.is_finite() {
                return Err(Error::Invalid("an opening receivable or payable is not a number".into()));
            }
            seq += 1;
            dues.push(Due {
                id: seq,
                k: records.calendar().times().partition_point(|x| *x < d.settles_at),
                slot,
                amount: Cash::from_f64(d.amount),
                income: Cash::ZERO,
            });
        }
        let dist = cfg.distributor.as_ref().map(|d| d.0.start());
        let st = LedgerState {
            k: 0,
            q,
            held,
            cash,
            interest_collections: vec![Cash::from_f64(0.0); nslots],
            dues,
            deliveries: Vec::new(),
            working: Vec::new(),
            queued: VecDeque::new(),
            weights: Vec::new(),
            units: cfg.initial_cash / 100.0,
            fx: vec![1.0; nslots],
            seq,
            collected: Collected::default(),
            dist,
            published: Vec::new(),
            agreements,
            pledged: vec![(0.0, usize::MAX); n],
            out: LedgerOutput {
                breaches,
                agreements: cfg.financing.iter().map(|a| AgreementPath { name: a.name.clone(), ..AgreementPath::default() }).collect(),
                ..LedgerOutput::default()
            },
        };
        let (before, after) = cfg.order.around_execution();
        let times = records.calendar().times();
        let transition_k = records.transitions().iter().map(|tr| times.partition_point(|x| *x < tr.at)).collect();
        Ok(Self {
            ws,
            records,
            policies: rts,
            order,
            exec,
            before,
            after,
            transition_k,
            quiet: false,
            cfg,
            slots,
            row_slot,
            settle_lag,
            fx_series,
            deposit,
            borrow_rate,
            available,
            flows,
            groups,
            fin,
            notes: Mutex::new(Vec::new()),
            st,
        })
    }

    /// Runs to the end of the calendar.
    ///
    /// # Errors
    /// See [`Ledger::step`].
    pub fn run(mut self) -> Result<LedgerOutput> {
        let times = self.records.calendar().times();
        let reporter = if self.quiet { Reporter::silent() } else { Reporter::start("fi.ledger", times.len() as u64, "steps") };
        while self.step()? {
            let k = self.st.k;
            reporter.report(k as u64, times.get(k - 1).copied());
        }
        reporter.report(times.len() as u64, times.last().copied());
        reporter.finish(true);
        Ok(self.finish())
    }

    /// The ledger, reporting no progress of its own: one of many in a sweep.
    #[must_use]
    pub fn quiet(mut self) -> Self {
        self.quiet = true;
        self
    }

    /// The accounts so far.
    #[must_use]
    pub fn output(&self) -> &LedgerOutput {
        &self.st.out
    }

    /// A checkpoint: the whole changing state.
    #[must_use]
    pub fn checkpoint(&self) -> LedgerState {
        self.st.clone()
    }

    /// Resumes from a checkpoint of a ledger built the same way.
    pub fn restore(&mut self, state: LedgerState) {
        self.st = state;
    }

    /// The currency of each cash slot (the base first).
    #[must_use]
    pub fn currencies(&self) -> &[EntityId] {
        &self.slots
    }

    /// The distributor's state now.
    #[must_use]
    pub fn distributor_state(&self) -> Option<&dyn DistributorState> {
        self.st.dist.as_deref()
    }

    /// The accounts, with the final holdings, cash and open orders.
    #[must_use]
    pub fn finish(mut self) -> LedgerOutput {
        let ents = self.records.entities();
        self.st.out.final_holdings = self.st.held.iter().map(|&i| (ents[i], self.st.q[i])).collect();
        // Legal ownership: the economic position less units bought and not yet delivered, plus
        // units sold and not yet delivered.
        let mut settled: Vec<(usize, f64)> = self.st.held.iter().map(|&i| (i, self.st.q[i])).collect();
        for &(i, qty, _) in &self.st.deliveries {
            match settled.iter_mut().find(|(r, _)| *r == i) {
                Some(row) => row.1 -= qty,
                None => settled.push((i, -qty)),
            }
        }
        settled.sort_by_key(|(i, _)| *i);
        self.st.out.final_settled = settled.into_iter().filter(|(_, q)| *q != 0.0).map(|(i, q)| (ents[i], q)).collect();
        for d in &self.st.dues {
            let v = d.amount.to_f64() * self.st.fx[d.slot];
            if v >= 0.0 {
                self.st.out.final_receivable += v;
            } else {
                self.st.out.final_payable -= v;
            }
        }
        self.st.out.final_cash = self.slots.iter().zip(&self.st.cash).map(|(s, c)| (*s, c.to_f64())).collect();
        self.st.out.pending_at_end = (self.st.queued.len() + self.st.working.len()) as u64;
        if self.cfg.journal {
            let open: Vec<OrderRecord> = self.st.working.iter().map(|o| record(o, None, OrderStatus::Open)).collect();
            self.st.out.order_log.extend(open);
        }
        self.st.out
    }

    fn value(&self, i: usize, k: usize) -> f64 {
        self.records.value[self.records.at(i, k)]
    }

    #[allow(clippy::too_many_arguments)]
    fn log(&mut self, t: Ts, kind: EntryKind, entity: EntityId, slot: usize, qty: f64, amount: f64, cause: Cause, note: Option<String>) -> u64 {
        // A counter in the state: the same run gives the same identifiers, and a run resumed from
        // a checkpoint continues the same sequence.
        self.st.seq += 1;
        let id = self.st.seq;
        if self.cfg.journal {
            let currency = self.slots[slot];
            self.st.out.journal.push(Entry { id, t, kind, entity, currency, qty, amount, cause, note });
        }
        id
    }

    fn nav(&self, k: usize) -> f64 {
        let st = &self.st;
        let mut v = 0.0;
        if self.slots.len() == 1 {
            for &i in &st.held {
                v += st.q[i] * self.value(i, k);
            }
        } else {
            for &i in &st.held {
                v += st.q[i] * self.value(i, k) * st.fx[self.row_slot[i]];
            }
        }
        for (s, x) in st.fx.iter().enumerate() {
            v += (st.cash[s].to_f64() + st.interest_collections[s].to_f64()) * x;
        }
        for d in &st.dues {
            v += d.amount.to_f64() * st.fx[d.slot];
        }
        // Secured borrowing is owed; collateral posted under an annex is a receivable (received:
        // a payable).
        for (a, s) in self.cfg.financing.iter().zip(&st.agreements) {
            v += match a.kind {
                AgreementKind::Secured { .. } => -s.balance,
                AgreementKind::Csa { .. } => s.balance,
            };
        }
        v
    }

    fn fx_at(&self, t: Ts) -> Result<Vec<f64>> {
        let mut out = vec![1.0; self.slots.len()];
        for (s, e) in self.slots.iter().enumerate().skip(1) {
            out[s] = read(self.fx_series, *e, t, Ts::MAX)
                .filter(|x| x.is_finite() && *x > 0.0)
                .ok_or_else(|| Error::Missing(format!("no FX rate for {} at {t}", self.ws.entities.name(*e))))?;
        }
        Ok(out)
    }

    /// Cash in `slot` that can be spent on something settling at step `by`: settled cash, plus
    /// receivables arriving by then, less every payable already committed.
    fn spendable(&self, slot: usize, by: usize) -> f64 {
        let mut c = self.st.cash[slot].to_f64();
        for d in &self.st.dues {
            if d.slot == slot {
                let a = d.amount.to_f64();
                if a < 0.0 || d.k <= by {
                    c += a;
                }
            }
        }
        c
    }

    /// How far below zero base cash may go.
    fn borrowable(&self) -> f64 {
        match &self.cfg.cash {
            CashPolicy::Borrow { limit, .. } => *limit,
            CashPolicy::Unlimited => f64::INFINITY,
            CashPolicy::Forbid { .. } => 0.0,
        }
    }

    fn hold(&mut self, mut rows: Vec<usize>) {
        rows.sort_unstable();
        rows.dedup();
        let q = &self.st.q;
        rows.retain(|&i| q[i] != 0.0);
        self.st.held = rows;
    }

    /// Advances one calendar time. Returns `false` when the calendar is exhausted.
    ///
    /// # Errors
    /// A decision naming an unknown contract or naming one twice, a missing FX rate, a breach of
    /// the identity, a guard set to fail, or a decision model's own error.
    pub fn step(&mut self) -> Result<bool> {
        let Some(mut s) = self.begin()? else { return Ok(false) };
        let (batch, values) = self.take_workable(s.k);
        let fills = if batch.is_empty() { Vec::new() } else { self.exec.work(s.t, &batch, &values) };
        self.apply(s.k, s.t, batch, &values, &fills, &mut s.acc)?;
        self.end(s)
    }

    /// The contract events and interest, then the declared phases before execution.
    fn begin(&mut self) -> Result<Option<Stepping>> {
        let times = self.records.calendar().times();
        let k = self.st.k;
        if k >= times.len() {
            return Ok(None);
        }
        let t = times[k];
        let mut acc = Acc::zero();
        let fx_now = self.fx_at(t)?;
        self.st.collected = Collected::default();
        if k > 0 {
            self.contract_events(k, t, &fx_now, &mut acc);
            self.interest(k, t, &mut acc, &fx_now);
        }
        self.st.fx = fx_now;
        if k > 0 {
            self.transitions(k, t, &mut acc)?;
        }
        let nav_prev = match self.st.out.nav.last() {
            Some(n) => *n,
            None => {
                // The opening NAV: cash and opening positions at the first time. A unit starts at 100.
                let open = self.nav(0);
                self.st.units = if open > 0.0 { open / 100.0 } else { 0.0 };
                open
            }
        };
        let mut s = Stepping { k, t, acc, nav_prev, nav_pre: None };
        for j in 0..self.before.len() {
            self.phase(self.before[j], &mut s)?;
        }
        Ok(Some(s))
    }

    /// The declared phases after execution, then the mark.
    fn end(&mut self, mut s: Stepping) -> Result<bool> {
        for j in 0..self.after.len() {
            self.phase(self.after[j], &mut s)?;
        }
        self.mark(s)
    }

    fn phase(&mut self, p: Phase, s: &mut Stepping) -> Result<()> {
        let (k, t) = (s.k, s.t);
        match p {
            Phase::WeightResets => {
                if self.cfg.holding == HoldingPolicy::ResetWeights && k > 0 && !self.st.weights.is_empty() {
                    self.reset_weights(k);
                }
            }
            Phase::CapitalFlows => {
                if s.nav_pre.is_none() {
                    s.nav_pre = Some(self.nav(k));
                }
                s.acc.capital += self.capital_flows(k, t)?;
            }
            Phase::Decisions => {
                let nav = match s.nav_pre {
                    Some(n) => n,
                    None => *s.nav_pre.insert(self.nav(k)),
                };
                if self.st.out.failed_at.is_none() {
                    self.decide(k, t, nav)?;
                }
                self.submit(k, t, nav)?;
            }
            // Execution runs between `begin` and `end`, so books run together can share it.
            Phase::Execution => {}
            Phase::Settlement => self.settle(k, t),
            Phase::Recalls => self.recalls(k, t),
            Phase::Distribution => s.acc.capital -= self.distribute(k, t)?,
            Phase::Agreements => self.agreements(k, t),
            Phase::CashGuard => self.check_cash(k, t, &mut s.acc)?,
            Phase::Constraints => {
                let nav = self.nav(k);
                self.check_constraints(k, t, nav)?;
            }
        }
        Ok(())
    }

    /// The mark: NAV, checked against the identity, and the step's accounts.
    fn mark(&mut self, s: Stepping) -> Result<bool> {
        let Stepping { k, t, acc, nav_prev, .. } = s;
        let nav = self.nav(k);
        let expected = if k == 0 {
            acc.capital - acc.costs
        } else {
            acc.income + acc.price + acc.principal + acc.other - acc.financing + acc.currency - acc.costs + acc.capital
        };
        let err = ((nav - nav_prev) - expected).abs();
        let rel = err / nav_prev.abs().max(1.0);
        self.st.out.max_identity_error = self.st.out.max_identity_error.max(rel);
        // A value that is not a number never passes the identity.
        if rel.is_nan() || rel > self.cfg.identity_tolerance {
            return Err(Error::Invalid(format!(
                "accounting identity broken at {t}: NAV moved {} but the terms add to {expected} (error {rel:e} of NAV)",
                nav - nav_prev
            )));
        }
        // Capital deals at the end-of-step NAV per unit, so it leaves NAV per unit unchanged.
        if acc.capital != 0.0 && self.st.units > 0.0 {
            let price = (nav - acc.capital) / self.st.units;
            if price > 0.0 {
                self.st.units += acc.capital / price;
            }
        }
        if nav <= 0.0 && self.st.out.failed_at.is_none() {
            self.st.out.failed_at = Some(t);
            self.log(t, EntryKind::Failure, EntityId::NONE, 0, 0.0, nav, Cause::Book, Some("NAV at or below zero".into()));
        }
        let npu = if self.st.units > 0.0 { nav / self.st.units } else { f64::NAN };
        let overdue: f64 = (0..self.slots.len()).map(|s| self.unmet(k, s) * self.st.fx[s]).sum();
        self.st.out.overdue.push(overdue);
        let cash: f64 = (0..self.slots.len()).map(|s| (self.st.cash[s].to_f64() + self.st.interest_collections[s].to_f64()) * self.st.fx[s]).sum();
        let unsettled: f64 = self.st.dues.iter().map(|d| d.amount.to_f64() * self.st.fx[d.slot]).sum();
        let held = self.st.held.len();
        let out = &mut self.st.out;
        out.times.push(t);
        out.nav.push(nav);
        out.cash.push(cash);
        out.income.push(acc.income);
        out.price.push(acc.price);
        out.principal.push(acc.principal);
        out.other.push(acc.other);
        out.financing.push(acc.financing);
        out.costs.push(acc.costs);
        out.turnover.push(acc.turnover);
        out.currency.push(acc.currency);
        out.capital.push(acc.capital);
        out.unsettled.push(unsettled);
        out.nav_per_unit.push(npu);
        out.holdings.push(held);
        for (p, s) in out.agreements.iter_mut().zip(&self.st.agreements) {
            p.balance.push(s.balance);
            p.capacity.push(s.capacity);
            p.call.push(s.call);
            p.called.push(s.called);
            p.paid.push(s.paid);
            p.value.push(s.value);
        }
        if self.cfg.keep_holdings {
            let ents = self.records.entities();
            let h = self.st.held.iter().map(|&i| (ents[i], self.st.q[i])).collect();
            self.st.out.holdings_path.push(h);
        }
        let last = k + 1 == self.records.calendar().len();
        if last && self.cfg.horizon == Horizon::Liquidate {
            self.liquidate(k, t, nav);
        }
        self.st.k += 1;
        Ok(!last)
    }
}

fn breach_of(c: &CashPolicy) -> Breach {
    match c {
        CashPolicy::Forbid { breach } => *breach,
        _ => Breach::Scale,
    }
}

fn record(o: &WorkingOrder, ended: Option<Ts>, status: OrderStatus) -> OrderRecord {
    OrderRecord {
        id: o.id,
        entity: o.entity,
        policy: o.policy,
        submitted: o.submitted,
        ended,
        requested: o.requested,
        filled: o.filled,
        average_price: if o.filled != 0.0 { o.paid / o.filled } else { f64::NAN },
        decision_value: o.decision_value,
        attempts: o.attempts,
        status,
    }
}

/// Runs a decision model over the records' calendar, filling through a fill model.
///
/// # Errors
/// See [`Ledger::step`].
pub fn run(
    ws: &Workspace,
    records: &UnitRecords,
    decide: &dyn DecisionModel,
    schedule: &[Ts],
    fill: &dyn FillModel,
    cfg: LedgerConfig,
) -> Result<LedgerOutput> {
    Ledger::new(ws, records, decide, schedule, fill, cfg)?.run()
}

/// Runs a book of several policies through an execution model.
///
/// # Errors
/// See [`Ledger::build`] and [`Ledger::step`].
pub fn run_policies(ws: &Workspace, records: &UnitRecords, policies: Vec<Policy<'_>>, exec: ExecRef<'_>, cfg: LedgerConfig) -> Result<LedgerOutput> {
    Ledger::build(ws, records, policies, exec, cfg)?.run()
}

/// Steps several books in lockstep through one execution model, which sees every book's orders
/// at each step together: the books compete for its capacity, and a capacity-limited model
/// allocates between them. The books must share a calendar.
///
/// # Errors
/// Books on different calendars, or any book's error.
pub fn run_together(books: Vec<Ledger<'_>>, shared: &dyn Execution) -> Result<Vec<LedgerOutput>> {
    let mut books = books;
    let Some(first) = books.first() else { return Ok(Vec::new()) };
    let cal = first.records.calendar().clone();
    if books.iter().any(|b| *b.records.calendar() != cal) {
        return Err(Error::Invalid("books run together must share a calendar".into()));
    }
    let reporter = Reporter::start("fi.ledger", cal.len() as u64, "steps");
    for k in 0..cal.len() {
        reporter.report(k as u64, cal.times().get(k).copied());
        let mut begun = Vec::with_capacity(books.len());
        let mut all = Vec::new();
        let mut values = Vec::new();
        let mut lens = Vec::with_capacity(books.len());
        for b in &mut books {
            let s = b.begin()?.ok_or_else(|| Error::Invalid("a book ended early".into()))?;
            let (batch, v) = b.take_workable(s.k);
            lens.push(batch.len());
            all.extend(batch);
            values.extend(v);
            begun.push(s);
        }
        let t = begun[0].t;
        let fills = if all.is_empty() { Vec::new() } else { shared.work(t, &all, &values) };
        let mut orders = all.into_iter();
        let mut from = 0;
        for (b, (mut s, len)) in books.iter_mut().zip(begun.into_iter().zip(lens)) {
            let batch: Vec<WorkingOrder> = orders.by_ref().take(len).collect();
            b.apply(s.k, s.t, batch, &values[from..from + len], &fills[from..from + len], &mut s.acc)?;
            from += len;
            b.end(s)?;
        }
    }
    reporter.report(cal.len() as u64, cal.times().last().copied());
    reporter.finish(true);
    Ok(books.into_iter().map(Ledger::finish).collect())
}
