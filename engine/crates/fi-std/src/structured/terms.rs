//! A deal's terms: its tranches and their coupons, fees, coverage tests, triggers, haircuts,
//! the steps of its priorities of payments, its losses, rights and exercises, and its manager.

use super::*;

/// How a tranche's coupon is set.
#[derive(Clone, Debug, PartialEq)]
pub enum Coupon {
    /// None: equity, or a principal-only claim.
    None,
    /// A fixed rate per year, as a fraction.
    Fixed(f64),
    /// A market-wide index (series, column) plus a spread, with an optional floor on the total.
    /// The index is read as known at the start of each accrual period.
    Floating {
        /// The index.
        index: (String, String),
        /// The spread over it, as a fraction.
        spread: f64,
        /// The lowest coupon.
        floor: Option<f64>,
    },
}

/// What happens to interest that is due and not paid. It never just disappears (F02).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Shortfall {
    /// Owed as arrears, and an event of default (a senior note that may not defer).
    Default,
    /// Deferred, and paid later in priority. If `compound`, it earns the coupon meanwhile.
    Defer {
        /// Interest on deferred interest, at the coupon.
        compound: bool,
    },
    /// Capitalised: added to the principal balance (PIK).
    Capitalise,
    /// Lost.
    Extinguish,
}

/// What a tranche is a claim to.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Claim {
    /// Interest on its balance, and its principal.
    Debt,
    /// What is left: no coupon and no principal claim.
    Equity,
    /// Interest on a reference notional, and no principal (F14). The notional is `ratio` times
    /// the pool's par or a tranche's balance. When it falls, the fall is written down, never paid.
    InterestOnly {
        /// The reference.
        of: Notional,
        /// Notional per unit of the reference.
        ratio: f64,
    },
}

/// The reference for an interest-only claim.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Notional {
    /// The collateral's par.
    Pool,
    /// A tranche's balance.
    Tranche(usize),
}

/// One of a deal's claims. A deal lists its tranches most senior first.
#[derive(Clone, Debug)]
pub struct Tranche {
    /// Name.
    pub name: String,
    /// Its entity: the contract an investor holds.
    pub entity: EntityId,
    /// Original face, or notional. One unit of the contract is one unit of it.
    pub face: f64,
    /// Balance at the deal's first time, for a seasoned deal (default: the face).
    pub balance: Option<f64>,
    /// The coupon.
    pub coupon: Coupon,
    /// Days in a year for accrual (Act/basis).
    pub basis: f64,
    /// What happens to unpaid interest.
    pub shortfall: Shortfall,
    /// What it is a claim to.
    pub claim: Claim,
    /// Outstanding from the first time. `false` for a replacement note, which an exercise
    /// issues.
    pub issued: bool,
    /// Scheduled balances, for a planned amortization class: (payment date, balance after it).
    pub schedule: Vec<(Ts, f64)>,
}

impl Tranche {
    /// A debt tranche of `face`: outstanding, accruing Act/360, and not allowed to defer.
    #[must_use]
    pub fn debt(name: &str, entity: EntityId, face: f64, coupon: Coupon) -> Self {
        Self {
            name: name.into(),
            entity,
            face,
            balance: None,
            coupon,
            basis: 360.0,
            shortfall: Shortfall::Default,
            claim: Claim::Debt,
            issued: true,
            schedule: Vec::new(),
        }
    }

    /// An equity tranche of `face`.
    #[must_use]
    pub fn equity(name: &str, entity: EntityId, face: f64) -> Self {
        Self { claim: Claim::Equity, shortfall: Shortfall::Extinguish, ..Self::debt(name, entity, face, Coupon::None) }
    }

    /// The scheduled balance at `t`, if a schedule entry applies.
    pub(super) fn scheduled(&self, t: Ts) -> Option<f64> {
        self.schedule.iter().rev().find(|(d, _)| *d <= t).map(|(_, b)| *b)
    }
}

/// A fee, paid where the waterfall says.
#[derive(Clone, Debug)]
pub struct Fee {
    /// Name.
    pub name: String,
    /// Its entity (the payee, in the distributions).
    pub entity: EntityId,
    /// Per year, on the collateral's par at the start of the period (Act/360).
    pub rate: f64,
    /// A fixed amount per payment date.
    pub fixed: f64,
    /// Carry unpaid amounts to the next payment date (otherwise they are lost).
    pub carry: bool,
}

/// The value a haircut carries a holding at, per unit of par.
#[derive(Clone, Debug, PartialEq)]
pub enum Carry {
    /// Its value per unit of par, from its unit records.
    Market,
    /// A column known at the test (a recovery rate, a purchase price), divided by `per` (100 for
    /// a price per 100).
    Column {
        /// The series.
        series: String,
        /// The column.
        column: String,
        /// The column's units per unit of par.
        per: f64,
    },
    /// A fixed fraction of par.
    Fixed(f64),
}

/// A rule of a deal's adjusted collateral (F01). It lowers the par counted for the holdings it
/// applies to. Rules apply in order, and a holding taken by one rule is not seen by the next.
#[derive(Clone, Debug)]
pub struct Haircut {
    /// Name.
    pub name: String,
    /// The holdings it applies to: a flag column (series, column), non-zero where it applies, as
    /// known at the test.
    pub when: (String, String),
    /// Each such holding counts at the lesser of these, per unit of par (and at most par).
    pub carry: Vec<Carry>,
    /// An excess bucket (excess CCC): only the part above this fraction of the remaining par is
    /// haircut, taking the holdings carried lowest first.
    pub excess_over: Option<f64>,
}

/// What a coverage test compares.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TestKind {
    /// Par coverage: the adjusted collateral (plus principal cash) over the classes' balances
    /// and deferred interest.
    Oc,
    /// Interest coverage: the interest collected less the fees paid so far, over the classes'
    /// interest for the period.
    Ic,
}

/// What a failing test diverts.
#[derive(Clone, Debug, PartialEq)]
pub enum Cure {
    /// Nothing: it is reported.
    None,
    /// Pay down these tranches in order, from the step's account, until the test passes.
    Paydown(Vec<usize>),
    /// Move interest into the principal account (to buy collateral) until the test passes.
    Reinvest,
}

/// A coverage test, run where the waterfall lists it.
#[derive(Clone, Debug)]
pub struct Test {
    /// Name.
    pub name: String,
    /// Its entity (in the tests series).
    pub entity: EntityId,
    /// What it compares.
    pub kind: TestKind,
    /// The classes in the denominator: this class and every senior one.
    pub classes: Vec<usize>,
    /// It passes at or above this.
    pub threshold: f64,
    /// What failing diverts.
    pub cure: Cure,
}

/// What a trigger measures on each payment date, before the waterfall runs.
#[derive(Clone, Debug, PartialEq)]
pub enum Metric {
    /// Cumulative losses over the original pool par.
    CumulativeLoss,
    /// The pool's par over its original par.
    PoolFactor,
    /// Interest collected in the period, less fees and interest due, over the pool's par, per
    /// year.
    ExcessSpread,
    /// Principal collected in the period over the pool's par at its start.
    PaymentRate,
    /// A coverage test's value.
    Test(usize),
    /// A market-wide series (series, column), as known.
    Series(String, String),
}

/// What a tripped trigger does, besides the steps conditioned on it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Nothing more.
    None,
    /// It ends reinvestment (an early amortization event).
    EndReinvestment,
}

/// A trigger: a persistent condition, evaluated at each payment date (F05, F18).
#[derive(Clone, Debug)]
pub struct Trigger {
    /// Name.
    pub name: String,
    /// Its entity (in the tests series).
    pub entity: EntityId,
    /// What it measures.
    pub metric: Metric,
    /// It trips when the averaged metric is above this...
    pub above: Option<f64>,
    /// ... or below this. Exactly at the boundary, it does not trip.
    pub below: Option<f64>,
    /// The number of payment dates averaged (1: this one only).
    pub lookback: usize,
    /// A curable trigger clears when the metric recovers. Otherwise it stays tripped.
    pub curable: bool,
    /// What it does.
    pub effect: Effect,
}

/// When a step runs.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum When {
    /// Always.
    Always,
    /// During reinvestment.
    Reinvesting,
    /// After reinvestment ends.
    Amortising,
    /// While a trigger is tripped.
    Tripped(usize),
    /// While a trigger is not tripped.
    NotTripped(usize),
}

/// One step of a priority of payments, paid from its list's account.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Pay a fee due, and any carried.
    Fee(usize),
    /// Pay the interest due and any deferred, pro rata across the tranches. If `reserve`, a
    /// shortfall is drawn from the reserve.
    Interest {
        /// The tranches, of equal priority.
        tranches: Vec<usize>,
        /// Draw a shortfall from the reserve.
        reserve: bool,
    },
    /// Pay principal to the tranches, in order or pro rata to their balances. If `schedule`, a
    /// tranche is paid only down to its scheduled balance.
    Principal {
        /// The tranches.
        tranches: Vec<usize>,
        /// Pro rata rather than in order.
        pro_rata: bool,
        /// Only down to the scheduled balances.
        schedule: bool,
    },
    /// Run a coverage test, and its cure if it fails.
    Test(usize),
    /// Top the reserve up to its target.
    Reserve,
    /// Cure these tranches' deficiency ledgers, in order, by moving interest into principal.
    Deficiency(Vec<usize>),
    /// Move what is left of interest into the principal account.
    ToPrincipal,
    /// Move what is left of principal into the interest account (a combined waterfall).
    ToInterest,
    /// Pay what is left to a tranche (the equity).
    Residual(usize),
}

/// A step and its condition.
#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    /// What it does.
    pub action: Action,
    /// When it runs.
    pub when: When,
}

impl Step {
    /// A step that always runs.
    #[must_use]
    pub fn always(action: Action) -> Self {
        Self { action, when: When::Always }
    }
}

/// Where losses go: principal outstanding that left the pool without cash.
#[derive(Clone, Debug, PartialEq)]
pub enum Losses {
    /// They stay in the collateral, where the coverage tests see them (a cash-flow CLO).
    Kept,
    /// Write the tranches down in this order, most junior first. If `write_up`, recoveries
    /// write them back up in reverse order.
    WriteDown {
        /// Most junior first.
        order: Vec<usize>,
        /// Recoveries reinstate written-down principal.
        write_up: bool,
    },
    /// Debit the tranches' deficiency ledgers in this order, most junior first, up to each
    /// balance. Recoveries credit them back in reverse order.
    Ledger(Vec<usize>),
}

/// An action a right allows.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RightKind {
    /// Redeem every note: sell the collateral at its value and pay by seniority.
    Call,
    /// Redeem classes at par, funded by replacement notes with new terms.
    Refinance,
    /// A refinancing that also changes the deal's terms (a new reinvestment end).
    Reset,
}

/// A right in the deal's terms (F04). A passive tranche holding confers none.
#[derive(Clone, Debug)]
pub struct Right {
    /// What it allows.
    pub kind: RightKind,
    /// The class that holds it (usually the equity).
    pub holder: usize,
    /// The share of that class that must consent.
    pub majority: f64,
    /// The end of the non-call period: it cannot be exercised before.
    pub from: Ts,
}

/// An exercise of a right, on a payment date.
#[derive(Clone, Debug)]
pub struct Exercise {
    /// When.
    pub t: Ts,
    /// What.
    pub kind: RightKind,
    /// The class exercising.
    pub by: usize,
    /// The share of that class that consents.
    pub share: f64,
    /// For a refinancing or a reset: the classes redeemed...
    pub redeem: Vec<usize>,
    /// ... and their replacements, one for one.
    pub replacements: Vec<usize>,
    /// Costs, paid from interest. Whatever interest cannot pay, the exercising class
    /// contributes.
    pub expenses: f64,
    /// A reset's new reinvestment end.
    pub reinvest_until: Option<Ts>,
}

/// A policy of the deal's manager, over the collateral book.
#[derive(Clone)]
pub struct Manager {
    /// Name.
    pub name: String,
    /// What it decides. It reads the deal's state through `ctx.state("reinvesting")`.
    pub decide: Arc<dyn DecisionModel>,
    /// When it decides.
    pub schedule: Vec<Ts>,
    /// Steps from decision to submission.
    pub lag: usize,
}

/// A securitisation: a collateral book and its priority of payments, evaluated as one batch
/// model. Its tranches are contracts on its output series (see the module documentation).
#[derive(Clone)]
pub struct Deal {
    /// Name: the prefix of its output series.
    pub name: String,
    /// The deal's entity (in the collateral series and the distributions).
    pub entity: EntityId,
    /// The collateral's contracts: the pool, and anything the manager may buy.
    pub collateral: Vec<Arc<dyn Contract>>,
    /// The opening pool: (contract, units).
    pub opening: Vec<(EntityId, f64)>,
    /// The opening principal account.
    pub cash: f64,
    /// The opening reserve.
    pub reserve: f64,
    /// The reserve's target.
    pub reserve_target: f64,
    /// Its claims, most senior first.
    pub tranches: Vec<Tranche>,
    /// Its fees.
    pub fees: Vec<Fee>,
    /// The adjusted collateral's rules.
    pub haircuts: Vec<Haircut>,
    /// Coverage tests.
    pub tests: Vec<Test>,
    /// Triggers.
    pub triggers: Vec<Trigger>,
    /// The interest priority of payments.
    pub interest: Vec<Step>,
    /// The principal priority of payments, run after the interest one.
    pub principal: Vec<Step>,
    /// Where losses go.
    pub losses: Losses,
    /// Payment dates: times of the calendar.
    pub payments: Vec<Ts>,
    /// The end of reinvestment.
    pub reinvest_until: Option<Ts>,
    /// The legal final maturity: the collateral is sold, and what is unpaid is written off.
    pub maturity: Option<Ts>,
    /// Rights in its terms.
    pub rights: Vec<Right>,
    /// Exercises of those rights.
    pub exercises: Vec<Exercise>,
    /// The manager's policies over the collateral.
    pub manager: Vec<Manager>,
    /// How the manager's orders fill.
    pub fill: Arc<dyn FillModel>,
    /// What the collateral does with missing values.
    pub missing: MissingPolicy,
    /// How the deal's payments become whole minor units, as its indenture says (F01: a cure is
    /// "before contractual rounding"). Amounts due round by the term's mode; a cure is the
    /// smallest whole amount that cures; no payment or transfer is a fraction of a unit, which
    /// stays in its account; a pro rata split goes in whole units, the largest remainders first.
    /// None: exact amounts.
    pub rounding: Option<Rounding>,
    /// The account entities: interest, principal, reserve.
    pub accounts: [EntityId; 3],
    /// Series read besides the contracts' and the terms' (a manager's inputs).
    pub reads: Vec<String>,
    /// Whether the graph may evaluate it on another thread (false with a Python manager).
    pub parallel: bool,
}
