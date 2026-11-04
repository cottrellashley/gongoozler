//! A book's settings: every assumption that would otherwise be silent is named here (spec,
//! changes 1, 2 and 5).

use std::any::Any;
use std::fmt;
use std::sync::Arc;

use super::order::StepOrder;
use crate::{
    Error, Result, cash::Rounding, entity::EntityId, hash::Hash64, measure::Measure, policy::HoldingPolicy, records::UnitRecords, time::Ts, view::View,
};

/// What the book's holdings paid and lost through their own events in one step, with the
/// quantities held over it, in the base currency: a collateral pool's collections.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct Collected {
    /// Income cash (interest).
    pub interest: f64,
    /// Every other cash flow: principal repaid, recoveries.
    pub principal: f64,
    /// Principal outstanding that left the holdings (repaid or written down).
    pub par_drop: f64,
}

impl Collected {
    /// Principal outstanding lost without cash: the part of the drop that no principal cash
    /// covered. Negative when cash arrives without a drop (a recovery after a write-off).
    #[must_use]
    pub fn loss(&self) -> f64 {
        self.par_drop - self.principal
    }
}

/// What a distributor sees and moves at one time (spec, change 3: a book is a contract). The
/// book's cash is two accounts in the base currency: interest collections and principal (the
/// spendable cash). Payments leave the book: they are its liabilities' flows, such as a deal's
/// distributions to its notes. Contributions come in from outside.
pub struct Distribution<'a> {
    /// The time.
    pub t: Ts,
    /// The calendar index.
    pub k: usize,
    /// The book's unit records.
    pub records: &'a UnitRecords,
    /// Data known now.
    pub view: View<'a>,
    /// The holdings: (record row, quantity).
    pub holdings: Vec<(usize, f64)>,
    /// What the holdings paid and lost in this step.
    pub collected: Collected,
    /// NAV before this distribution, in the base currency.
    pub nav: f64,
    pub(crate) interest: f64,
    pub(crate) principal: f64,
    pub(crate) paid: f64,
    pub(crate) liquidated: bool,
    pub(crate) fx: Vec<f64>,
    pub(crate) row_slot: Vec<usize>,
    pub(crate) notes: Vec<String>,
    pub(crate) published: Vec<(String, f64)>,
}

impl Distribution<'_> {
    /// Cash in the interest collection account.
    #[must_use]
    pub fn interest(&self) -> f64 {
        self.interest
    }

    /// Cash in the principal account.
    #[must_use]
    pub fn principal(&self) -> f64 {
        self.principal
    }

    /// Paid out (net of contributions) so far in this distribution.
    #[must_use]
    pub fn paid(&self) -> f64 {
        self.paid
    }

    /// Pays up to `amount` out of interest; returns what was paid.
    pub fn pay_interest(&mut self, amount: f64) -> f64 {
        let p = amount.max(0.0).min(self.interest.max(0.0));
        self.interest -= p;
        self.paid += p;
        p
    }

    /// Pays up to `amount` out of principal; returns what was paid.
    pub fn pay_principal(&mut self, amount: f64) -> f64 {
        let p = amount.max(0.0).min(self.principal.max(0.0));
        self.principal -= p;
        self.paid += p;
        p
    }

    /// Moves up to `amount` of interest into the principal account: kept in the book, as interest
    /// diverted to buy collateral. Returns what moved.
    pub fn interest_to_principal(&mut self, amount: f64) -> f64 {
        let m = amount.max(0.0).min(self.interest.max(0.0));
        self.interest -= m;
        self.principal += m;
        m
    }

    /// Moves up to `amount` of principal into the interest account. Returns what moved.
    pub fn principal_to_interest(&mut self, amount: f64) -> f64 {
        let m = amount.max(0.0).min(self.principal.max(0.0));
        self.principal -= m;
        self.interest += m;
        m
    }

    /// Puts `amount` into the interest account from outside the book (a reserve draw, an equity
    /// contribution).
    pub fn contribute_interest(&mut self, amount: f64) {
        self.interest += amount;
        self.paid -= amount;
    }

    /// Principal outstanding of the holdings: Σ quantity × face per unit, in the base currency.
    #[must_use]
    pub fn par(&self) -> f64 {
        self.holdings.iter().map(|(i, q)| q * self.records.face[self.records.at(*i, self.k)] * self.fx[self.row_slot[*i]]).filter(|x| x.is_finite()).sum()
    }

    /// Principal outstanding of the holding at position `pos` of [`Distribution::holdings`], in
    /// the base currency (NaN where the contract has no principal).
    #[must_use]
    pub fn par_of(&self, pos: usize) -> f64 {
        let (i, q) = self.holdings[pos];
        q * self.records.face[self.records.at(i, self.k)] * self.fx[self.row_slot[i]]
    }

    /// Market value of the holding at position `pos`, in the base currency.
    #[must_use]
    pub fn value_of(&self, pos: usize) -> f64 {
        let (i, q) = self.holdings[pos];
        q * self.records.value[self.records.at(i, self.k)] * self.fx[self.row_slot[i]]
    }

    /// Market value of the holdings, in the base currency.
    #[must_use]
    pub fn value(&self) -> f64 {
        self.holdings.iter().map(|(i, q)| q * self.records.value[self.records.at(*i, self.k)] * self.fx[self.row_slot[*i]]).sum()
    }

    /// The holdings' exposure in a measure, in the base currency.
    #[must_use]
    pub fn exposure(&self, m: &dyn Measure) -> f64 {
        self.holdings.iter().filter_map(|(i, q)| m.per_unit(self.records, *i, self.k, &self.view).map(|x| q * x * self.fx[self.row_slot[*i]])).sum()
    }

    /// Sells every holding at its value now into the principal account, as a deal's terms
    /// allow (an optional redemption, a clean-up call, the legal final maturity). Returns the
    /// proceeds.
    pub fn liquidate(&mut self) -> f64 {
        let v = self.value();
        self.principal += v;
        self.holdings.clear();
        self.liquidated = true;
        v
    }

    /// Publishes a value of the book's own state (`"reinvesting"`): its policies read it from
    /// the next decision on.
    pub fn publish(&mut self, key: &str, value: f64) {
        match self.published.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = value,
            None => self.published.push((key.to_owned(), value)),
        }
    }

    /// Records a note in the journal.
    pub fn note(&mut self, note: impl Into<String>) {
        self.notes.push(note.into());
    }
}

/// A distributor's own state (balances, deferred amounts, triggers), kept in the book's state so
/// that a checkpoint holds it. Any `Clone + Send + Sync` type is one.
pub trait DistributorState: Send + Sync {
    /// A boxed copy.
    fn clone_state(&self) -> Box<dyn DistributorState>;
    /// The state, for downcasting.
    fn state_any(&self) -> &dyn Any;
    /// The state, for downcasting.
    fn state_any_mut(&mut self) -> &mut dyn Any;
}

impl<T: Any + Clone + Send + Sync> DistributorState for T {
    fn clone_state(&self) -> Box<dyn DistributorState> {
        Box::new(self.clone())
    }

    fn state_any(&self) -> &dyn Any {
        self
    }

    fn state_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// Pays out of a book's accounts at each time: an allocation rule, such as a securitisation's
/// priority of payments over its collateral book (spec, change 3). It runs after settlement and
/// before the cash guard and the mark.
pub trait Distributor: Send + Sync {
    /// Name.
    fn name(&self) -> String;
    /// The state it starts from.
    fn start(&self) -> Box<dyn DistributorState>;
    /// Pays, moves or contributes at time `d.t`.
    ///
    /// # Errors
    /// Whatever it cannot do (a forbidden action, a broken rule): the run stops.
    fn distribute(&self, d: &mut Distribution<'_>, state: &mut dyn DistributorState) -> Result<()>;
}

/// A distributor, shared.
#[derive(Clone)]
pub struct DistributorRef(pub Arc<dyn Distributor>);

impl fmt::Debug for DistributorRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Distributor({})", self.0.name())
    }
}

/// What happens when a buy needs more cash than the book has.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum Breach {
    /// Buy what the cash allows (counted).
    #[default]
    Scale,
    /// Do not buy (counted).
    Refuse,
    /// Fail the run.
    Fail,
}

/// How a book may use cash (spec, change 1). Unlimited, free borrowing is an assumption, so it
/// is never the default.
#[derive(Clone, Debug, PartialEq)]
pub enum CashPolicy {
    /// Cash may not fall below zero in any currency: a mandatory, hard guard.
    Forbid {
        /// What a buy that would breach it does.
        breach: Breach,
    },
    /// Borrow in the base currency up to `limit`, paying `rate` (a market-wide series, Act/360)
    /// as financing.
    Borrow {
        /// Largest negative base-currency balance.
        limit: f64,
        /// The borrowing rate (series, column); none: free.
        rate: Option<(String, String)>,
    },
    /// Unlimited, free borrowing: the legacy assumption, reported as such.
    Unlimited,
}

impl Default for CashPolicy {
    fn default() -> Self {
        Self::Forbid { breach: Breach::Scale }
    }
}

/// How a book may sell securities it does not hold.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum ShortPolicy {
    /// No short securities: a sale is cut to what is held (counted). Derivatives are unaffected.
    #[default]
    Forbid,
    /// Borrow securities to short, at a fee, up to what is available.
    Borrow {
        /// Annual fee in basis points of the borrowed value (Act/360).
        fee_bp: f64,
        /// Units available to borrow per entity (series, column); none: unlimited. A fall below the
        /// short position is a recall, bought back at the next execution.
        available: Option<(String, String)>,
    },
    /// Unlimited free borrowing: an assumption, reported as such.
    Unlimited,
}

/// When trades settle: in calendar steps after the trade.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settlement {
    /// The default lag.
    pub lag: usize,
    /// Lags by contract.
    pub by_contract: Vec<(EntityId, usize)>,
}

/// What a run does with its holdings at the last time.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum Horizon {
    /// Value them at their marks.
    #[default]
    Mark,
    /// Sell them through the run's own execution model, with its costs; what does not sell is
    /// reported as a residual.
    Liquidate,
}

/// Where a book's cash flows go.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum Routing {
    /// Everything into cash.
    #[default]
    Cash,
    /// Income into an interest collection account, everything else into cash (the principal
    /// collection account, which also pays for purchases): how a securitisation's collateral book
    /// works.
    Collections,
}

/// What a capital-flow model sees: the book's own history up to the previous time.
pub struct FlowCtx<'a> {
    /// The time.
    pub t: Ts,
    /// The calendar index.
    pub k: usize,
    /// NAV at each earlier time.
    pub nav: &'a [f64],
    /// NAV per unit at each earlier time.
    pub nav_per_unit: &'a [f64],
    /// Data known now.
    pub view: View<'a>,
}

/// Subscriptions and redemptions that depend on the book itself (redemptions that follow
/// performance): a feedback loop inside the book, causal because it reads only the past.
pub trait FlowModel: Send + Sync {
    /// Name.
    fn name(&self) -> String;
    /// The flow now, in the base currency: positive a subscription, negative a redemption.
    ///
    /// # Errors
    /// Whatever the model cannot compute.
    fn flow(&self, ctx: &FlowCtx<'_>) -> Result<f64>;
}

/// External capital: excluded from P&L, and unitised so returns are time-weighted.
#[derive(Clone)]
pub enum CapitalFlows {
    /// Observed flows: a market-wide series of amounts in the base currency, read when known,
    /// paid `settle` steps later.
    Series {
        /// Series.
        series: String,
        /// Column.
        column: String,
        /// Steps until the cash moves.
        settle: usize,
    },
    /// Modelled flows.
    Model {
        /// The model.
        model: Arc<dyn FlowModel>,
        /// Steps until the cash moves.
        settle: usize,
    },
}

impl fmt::Debug for CapitalFlows {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Series { series, column, settle } => write!(f, "Series({series}.{column}, settle {settle})"),
            Self::Model { model, settle } => write!(f, "Model({}, settle {settle})", model.name()),
        }
    }
}

/// How cash in one currency is obtained from another when a trade needs it.
#[derive(Clone, Debug, PartialEq)]
pub struct FxConversion {
    /// Convert base currency automatically to pay for a trade in another currency.
    pub auto: bool,
    /// The cost of a conversion, in basis points.
    pub spread_bp: f64,
}

impl Default for FxConversion {
    fn default() -> Self {
        Self { auto: true, spread_bp: 0.0 }
    }
}

/// Which contracts a constraint covers.
#[derive(Clone, Debug, PartialEq)]
pub enum ConstraintScope {
    /// The whole book.
    Book,
    /// These contracts.
    Contracts(Vec<EntityId>),
    /// Each group separately, the group of a contract read from `series.column` (a category or a
    /// reference: sector, issuer, country).
    PerGroup {
        /// Series.
        series: String,
        /// Column.
        column: String,
    },
}

/// A mandate rule on the book's exposure in a measure, checked after fills and funding at every
/// time (spec, change 2). Hard rules are never relaxed; soft ones carry a penalty.
#[derive(Clone)]
pub struct Constraint {
    /// Name.
    pub name: String,
    /// The measure per unit (value, DTS, DV01, face…).
    pub measure: Arc<dyn Measure>,
    /// What it covers.
    pub scope: ConstraintScope,
    /// Lower bound.
    pub min: Option<f64>,
    /// Upper bound.
    pub max: Option<f64>,
    /// Bounds are fractions of NAV.
    pub relative: bool,
    /// A hard rule.
    pub hard: bool,
    /// A legal or mandate rule: it must be hard.
    pub mandatory: bool,
    /// The penalty per unit of excess, for a soft rule.
    pub penalty: Option<f64>,
    /// A hard breach fails the run, rather than being reported.
    pub fail: bool,
}

impl fmt::Debug for Constraint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Constraint({}, {}, {:?}, min {:?}, max {:?}, hard {})", self.name, self.measure.name(), self.scope, self.min, self.max, self.hard)
    }
}

impl Constraint {
    /// A hard rule.
    #[must_use]
    pub fn hard(name: &str, measure: Arc<dyn Measure>, scope: ConstraintScope, min: Option<f64>, max: Option<f64>) -> Self {
        Self { name: name.to_owned(), measure, scope, min, max, relative: false, hard: true, mandatory: false, penalty: None, fail: false }
    }

    /// A soft preference with a penalty per unit of excess.
    #[must_use]
    pub fn soft(name: &str, measure: Arc<dyn Measure>, scope: ConstraintScope, min: Option<f64>, max: Option<f64>, penalty: f64) -> Self {
        Self { hard: false, penalty: Some(penalty), ..Self::hard(name, measure, scope, min, max) }
    }

    /// Bounds as fractions of NAV.
    #[must_use]
    pub fn relative(mut self) -> Self {
        self.relative = true;
        self
    }

    /// A legal or mandate rule (it must be hard).
    #[must_use]
    pub fn mandatory(mut self) -> Self {
        self.mandatory = true;
        self
    }

    /// A hard breach fails the run.
    #[must_use]
    pub fn failing(mut self) -> Self {
        self.fail = true;
        self
    }

    /// Checks the rule's own consistency: a mandatory rule is hard, a hard rule has no penalty, a
    /// soft rule has a positive penalty, and the bounds are ordered.
    ///
    /// # Errors
    /// Any of those broken.
    pub fn validate(&self) -> Result<()> {
        let n = &self.name;
        if self.mandatory && !self.hard {
            return Err(Error::Invalid(format!("constraint {n} is mandatory, so it cannot become a penalty")));
        }
        if self.hard && self.penalty.is_some() {
            return Err(Error::Invalid(format!("constraint {n} is hard, so it has no penalty")));
        }
        if !self.hard && !self.penalty.is_some_and(|p| p > 0.0) {
            return Err(Error::Invalid(format!("constraint {n} is soft, so it needs a positive penalty")));
        }
        if let (Some(lo), Some(hi)) = (self.min, self.max)
            && lo > hi
        {
            return Err(Error::Invalid(format!("constraint {n}: min {lo} is above max {hi}")));
        }
        if self.min.is_none() && self.max.is_none() {
            return Err(Error::Invalid(format!("constraint {n} has no bound")));
        }
        Ok(())
    }
}

/// What a secured agreement advances against.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum Basis {
    /// The collateral's market value (its unit records' value).
    #[default]
    Market,
    /// Its principal outstanding (its unit records' face).
    Par,
}

/// The fraction of a holding's value an agreement lends against: one less its haircut.
#[derive(Clone, Debug, PartialEq)]
pub enum Advance {
    /// Fixed.
    Fixed(f64),
    /// A series of the contract's advance rate (series, column), as the agreement sets it over
    /// time: a haircut increase, or zero when the holding stops being eligible.
    Series(String, String),
}

/// What an agreement is.
#[derive(Clone, Debug, PartialEq)]
pub enum AgreementKind {
    /// Secured borrowing: a repo, a warehouse facility, a borrowing base. The book may borrow up
    /// to the advance value of its eligible holdings, which it pledges. Collateral pledged to an
    /// earlier agreement cannot support this one (encumbrance). A higher haircut lowers the
    /// capacity: a funding event, not a loss. A balance above capacity is a margin call, due
    /// `cure` steps later; at `maturity` the whole balance is due, and nothing extends it.
    Secured {
        /// The contracts it may finance, with their advance rates.
        collateral: Vec<(EntityId, Advance)>,
        /// The value it advances against.
        basis: Basis,
        /// No holding counts for more than this fraction of the aggregate eligible collateral.
        concentration: Option<f64>,
        /// The committed amount: the largest balance.
        limit: f64,
        /// Interest on the balance: a market-wide rate (series, column), Act/360.
        rate: Option<(String, String)>,
        /// When the balance must be repaid.
        maturity: Option<Ts>,
        /// Steps a margin call may stay unpaid.
        cure: usize,
        /// The balance at the first time.
        opening: f64,
        /// Draw on it to meet a cash need (when the responses list `Draw`).
        draw: bool,
        /// Repay it from free cash at each step.
        sweep: bool,
        /// When the lender assesses margin (its margin frequency): calls are made only then. None:
        /// at every step.
        margin: Option<Vec<Ts>>,
        /// How capacity becomes whole minor units, as the agreement says. None: exact.
        rounding: Option<Rounding>,
    },
    /// A credit support annex over one netting set: the set's value beyond `threshold` is
    /// collateralised in cash every `every` steps, in moves of at least `minimum`. Posting cash
    /// creates a receivable, not a loss; receiving it, a payable. Separate annexes never net.
    Csa {
        /// The netting set.
        set: Vec<EntityId>,
        /// Uncollateralised exposure allowed.
        threshold: f64,
        /// The smallest transfer.
        minimum: f64,
        /// Steps between margin calls.
        every: usize,
    },
}

/// A financing agreement between the book and a counterparty (spec, change 5): its terms here,
/// its state (balance, calls, posted collateral) in the book's state.
#[derive(Clone, Debug, PartialEq)]
pub struct Agreement {
    /// Name.
    pub name: String,
    /// Its terms.
    pub kind: AgreementKind,
}

/// A receivable (positive) or payable (negative) a book opens with: a trade or a payment already
/// in flight at the first time, settling at an instant.
#[derive(Clone, Debug, PartialEq)]
pub struct OpeningDue {
    /// Its currency (`EntityId::NONE`: the base currency).
    pub currency: EntityId,
    /// Owed to the book (positive) or by it (negative).
    pub amount: f64,
    /// When it settles.
    pub settles_at: Ts,
}

/// How a book meets a cash need, in order (spec, change 5). What none of them covers is an
/// economic failure.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Response {
    /// Exchange base cash for other currencies short of cash (if conversion is automatic).
    Convert,
    /// Draw on the secured agreements that allow it, in order.
    Draw,
    /// Sell holdings at their value less `cost_bp`: unpledged ones first; pledged collateral
    /// last, repaying its agreement from the proceeds (a closeout).
    Sell {
        /// The cost of selling, in basis points of value.
        cost_bp: f64,
    },
}

/// Ledger settings.
#[derive(Clone, Debug)]
pub struct LedgerConfig {
    /// Starting cash, in the base currency.
    pub initial_cash: f64,
    /// How holdings evolve between decisions.
    pub holding: HoldingPolicy,
    /// Periods between a decision and its submission (for the single-policy constructors).
    pub execution_lag: usize,
    /// Fail the run if the identity breaks by more than this fraction of NAV.
    pub identity_tolerance: f64,
    /// The base currency (an entity of kind `currency`); `EntityId::NONE`: one implicit currency.
    pub base_currency: EntityId,
    /// FX rates: base units per unit of each currency, a series keyed by currency entities.
    pub fx: Option<(String, String)>,
    /// How cash in a trade's currency is obtained.
    pub conversion: FxConversion,
    /// How the book may use cash.
    pub cash: CashPolicy,
    /// A market-wide rate earned on positive base-currency cash (Act/360); none: cash earns nothing.
    pub deposit_rate: Option<(String, String)>,
    /// How the book may short securities.
    pub shorts: ShortPolicy,
    /// When trades settle.
    pub settlement: Settlement,
    /// What happens at the last time.
    pub horizon: Horizon,
    /// Subscriptions and redemptions.
    pub flows: Option<CapitalFlows>,
    /// Steps an order works before it expires (1: immediate or cancel).
    pub time_in_force: usize,
    /// Mandate rules.
    pub constraints: Vec<Constraint>,
    /// Where cash flows go.
    pub routing: Routing,
    /// Keep a journal of every transfer with its cause.
    pub journal: bool,
    /// Keep the holdings at every time (for revaluing the path under other marks).
    pub keep_holdings: bool,
    /// Positions held from the first time without trading: a deal's opening collateral, a
    /// book's actual holdings at a branch.
    pub opening: Vec<(EntityId, f64)>,
    /// Receivables and payables in flight at the first time.
    pub opening_dues: Vec<OpeningDue>,
    /// Pays out of the book's accounts at each time (a deal's waterfall).
    pub distributor: Option<DistributorRef>,
    /// Financing agreements, in priority order (an earlier one is pledged collateral first).
    pub financing: Vec<Agreement>,
    /// How a cash need is met, in order.
    pub responses: Vec<Response>,
    /// The order of a step's phases between the contract events and the mark.
    pub order: StepOrder,
}

impl Default for LedgerConfig {
    fn default() -> Self {
        Self {
            initial_cash: 1e8,
            holding: HoldingPolicy::Drift,
            execution_lag: 0,
            identity_tolerance: 1e-9,
            base_currency: EntityId::NONE,
            fx: None,
            conversion: FxConversion::default(),
            cash: CashPolicy::default(),
            deposit_rate: None,
            shorts: ShortPolicy::default(),
            settlement: Settlement::default(),
            horizon: Horizon::Mark,
            flows: None,
            time_in_force: 1,
            constraints: Vec::new(),
            routing: Routing::Cash,
            journal: false,
            keep_holdings: false,
            opening: Vec::new(),
            opening_dues: Vec::new(),
            distributor: None,
            financing: Vec::new(),
            responses: vec![Response::Convert, Response::Draw],
            order: StepOrder::default(),
        }
    }
}

fn pair(h: &mut Hash64, p: Option<&(String, String)>) {
    match p {
        Some((s, c)) => {
            h.str(s);
            h.str(c);
        }
        None => h.u64(0),
    }
}

impl LedgerConfig {
    /// A hash of every setting (for strategy fingerprints and manifests).
    #[must_use]
    pub fn fingerprint(&self) -> u64 {
        let mut h = Hash64::new();
        h.f64(self.initial_cash);
        h.u64(self.holding as u64);
        h.u64(self.execution_lag as u64);
        h.u64(u64::from(self.base_currency.0));
        pair(&mut h, self.fx.as_ref());
        h.u64(u64::from(self.conversion.auto));
        h.f64(self.conversion.spread_bp);
        match &self.cash {
            CashPolicy::Forbid { breach } => {
                h.u64(1);
                h.u64(*breach as u64);
            }
            CashPolicy::Borrow { limit, rate } => {
                h.u64(2);
                h.f64(*limit);
                pair(&mut h, rate.as_ref());
            }
            CashPolicy::Unlimited => h.u64(3),
        }
        pair(&mut h, self.deposit_rate.as_ref());
        match &self.shorts {
            ShortPolicy::Forbid => h.u64(1),
            ShortPolicy::Borrow { fee_bp, available } => {
                h.u64(2);
                h.f64(*fee_bp);
                pair(&mut h, available.as_ref());
            }
            ShortPolicy::Unlimited => h.u64(3),
        }
        h.u64(self.settlement.lag as u64);
        for (e, l) in &self.settlement.by_contract {
            h.u64(u64::from(e.0));
            h.u64(*l as u64);
        }
        h.u64(self.horizon as u64);
        h.str(&format!("{:?}", self.flows));
        h.u64(self.time_in_force as u64);
        for c in &self.constraints {
            h.str(&format!("{c:?}"));
        }
        h.u64(self.routing as u64);
        for (e, q) in &self.opening {
            h.u64(u64::from(e.0));
            h.f64(*q);
        }
        h.str(&format!("{:?}", self.opening_dues));
        if let Some(d) = &self.distributor {
            h.str(&d.0.name());
        }
        h.str(&format!("{:?}", (&self.financing, &self.responses)));
        // The default order hashes as nothing, so books that never declared one keep their prints.
        if self.order != StepOrder::default() {
            for p in self.order.phases() {
                h.str(p.name());
            }
        }
        h.finish()
    }
}
