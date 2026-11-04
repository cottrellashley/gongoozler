//! Structured products: a deal is a book whose liabilities are contracts (spec, change 3).
//!
//! A [`Deal`] is a batch model. It runs its collateral as a book on the kernel's ledger: the
//! opening pool, the manager's policies and execution, and interest and principal collected
//! into separate accounts. A [`Waterfall`] is that book's distributor. It is the deal's own
//! priority of payments: two lists of steps, over the interest and the principal account, run
//! in order on each payment date. The engine imposes no order of its own. The waterfall's state
//! is held in the book's state, so a checkpoint holds it: tests, triggers, deferred interest, the
//! reserve and the deficiency ledgers.
//!
//! The deal writes its tranches' flows and marks as series. A tranche is then a
//! [`SuppliedBond`](crate::SuppliedBond) on them, held by any book like any other bond.
//!
//! * `{deal}.flows`: per tranche and payment date, per unit of original face: `interest`,
//!   `principal_face`, `writedown_face`, `writeup_face`, `pik_face`.
//! * `{deal}.marks`: per tranche and time:
//!   - `clean` and `accrued` per 100 of current face: debt at par with its accrued and deferred
//!     interest, equity at book value, an interest-only claim at zero;
//!   - also `factor`, `balance`, `deferred` and `deficiency`.
//! * `{deal}.distributions`: the cash paid to each tranche and fee on each payment date.
//! * `{deal}.tests`: each coverage test and trigger on each payment date: `value`, `threshold`,
//!   `passed`, `cure` (the cash diverted), `after` (the value after the cure) and `passed_after`.
//! * `{deal}.accounts`: the interest, principal and reserve accounts at each time.
//! * `{deal}.collateral`: the pool's `par` and `value`, the `interest` and `principal`
//!   collected, cumulative `losses`, the deal's `nav`, and whether it is `reinvesting`.
//! * `{deal}.claims`: the replacement notes a refinancing issued, each with the claim it
//!   `replaces` and its opening `balance`.

use std::collections::BTreeSet;
use std::sync::Arc;

use fi_kernel::{
    BatchModel, Calendar, Collected, Column, Contract, DecisionModel, Distribution, Distributor, DistributorRef, DistributorState, EntityId, Error, ExecRef,
    FillModel, Intervention, KnowledgeBasis, Ledger, LedgerConfig, MissingPolicy, ModelSpec, Policy, Producer, Provenance, Result, Rounding, Routing, Series,
    Shape, Ts, Unit, UnitRecords, Workspace, hash::Hash64,
};

const TINY: f64 = 1e-9;

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
    fn scheduled(&self, t: Ts) -> Option<f64> {
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

impl Deal {
    /// The series it writes, in order.
    #[must_use]
    pub fn outputs(&self) -> Vec<String> {
        ["flows", "marks", "distributions", "tests", "accounts", "collateral", "claims"].iter().map(|w| format!("{}.{w}", self.name)).collect()
    }

    fn fail<T>(&self, m: impl std::fmt::Display) -> Result<T> {
        Err(Error::Invalid(format!("deal {}: {m}", self.name)))
    }

    /// Checks the terms: every index in range, steps that fit their lists, and rights that
    /// allow each exercise (F04: an exercise before the non-call date, or without the right or
    /// its majority, is refused).
    ///
    /// # Errors
    /// The first inconsistency.
    pub fn validate(&self) -> Result<()> {
        let n = self.tranches.len();
        if n == 0 {
            return self.fail("no tranches");
        }
        let tranche = |j: usize| -> Result<()> { if j < n { Ok(()) } else { self.fail(format!("tranche {j} does not exist")) } };
        for tr in &self.tranches {
            if !(tr.face.is_finite() && tr.face > 0.0) {
                return self.fail(format!("{} has face {}", tr.name, tr.face));
            }
            if tr.balance.is_some_and(|b| !b.is_finite() || b < 0.0) || tr.basis.is_nan() || tr.basis <= 0.0 {
                return self.fail(format!("{} has an invalid balance or day basis", tr.name));
            }
            if let Claim::InterestOnly { of, ratio } = tr.claim {
                if !ratio.is_finite() {
                    return self.fail(format!("{} has notional ratio {ratio}", tr.name));
                }
                if let Notional::Tranche(i) = of {
                    tranche(i)?;
                }
            }
            if tr.schedule.windows(2).any(|w| w[0].0 >= w[1].0) {
                return self.fail(format!("{}'s schedule is not in time order", tr.name));
            }
        }
        for (list, steps) in [("interest", &self.interest), ("principal", &self.principal)] {
            for s in steps {
                match s.when {
                    When::Tripped(i) | When::NotTripped(i) if i >= self.triggers.len() => return self.fail(format!("trigger {i} does not exist")),
                    _ => {}
                }
                match &s.action {
                    Action::Fee(f) if *f >= self.fees.len() => return self.fail(format!("fee {f} does not exist")),
                    Action::Interest { tranches, .. } => tranches.iter().try_for_each(|&j| tranche(j))?,
                    Action::Principal { tranches, .. } => {
                        for &j in tranches {
                            tranche(j)?;
                            if self.tranches[j].claim != Claim::Debt {
                                return self.fail(format!("{} has no principal claim", self.tranches[j].name));
                            }
                        }
                    }
                    Action::Test(i) => {
                        let t = self.tests.get(*i).ok_or_else(|| Error::Invalid(format!("deal {}: test {i} does not exist", self.name)))?;
                        if t.cure == Cure::Reinvest && (t.kind != TestKind::Oc || list != "interest") {
                            return self.fail(format!("test {} cures by reinvesting, which only a par test in the interest list can", t.name));
                        }
                    }
                    Action::Deficiency(js) => {
                        js.iter().try_for_each(|&j| tranche(j))?;
                        if list != "interest" {
                            return self.fail("deficiency ledgers are cured from interest");
                        }
                    }
                    Action::ToPrincipal if list != "interest" => return self.fail("moving interest to principal is an interest step"),
                    Action::ToInterest if list != "principal" => return self.fail("moving principal to interest is a principal step"),
                    Action::Residual(j) => tranche(*j)?,
                    _ => {}
                }
            }
        }
        for t in &self.tests {
            t.classes.iter().try_for_each(|&j| tranche(j))?;
            if let Cure::Paydown(order) = &t.cure {
                order.iter().try_for_each(|&j| tranche(j))?;
            }
            if !(t.threshold.is_finite() && t.threshold > 0.0) {
                return self.fail(format!("test {} has threshold {}", t.name, t.threshold));
            }
        }
        for tr in &self.triggers {
            if tr.above.is_none() && tr.below.is_none() {
                return self.fail(format!("trigger {} has no threshold", tr.name));
            }
            if let Metric::Test(i) = tr.metric
                && i >= self.tests.len()
            {
                return self.fail(format!("trigger {} reads test {i}, which does not exist", tr.name));
            }
        }
        match &self.losses {
            Losses::Kept => {}
            Losses::WriteDown { order, .. } | Losses::Ledger(order) => order.iter().try_for_each(|&j| tranche(j))?,
        }
        for h in &self.haircuts {
            if h.carry.is_empty() {
                return self.fail(format!("haircut {} carries at nothing", h.name));
            }
            if h.excess_over.is_some_and(|x| !(0.0..=1.0).contains(&x)) {
                return self.fail(format!("haircut {}'s excess bucket is not a fraction", h.name));
            }
        }
        if self.payments.windows(2).any(|w| w[0] >= w[1]) {
            return self.fail("payment dates are not strictly increasing");
        }
        if let Some(m) = self.maturity
            && self.payments.binary_search(&m).is_err()
        {
            return self.fail(format!("the maturity {m} is not a payment date"));
        }
        if self.rounding.is_some_and(|r| r.unit.is_nan() || r.unit <= 0.0) {
            return self.fail("the rounding unit is not positive");
        }
        if !(self.cash.is_finite() && self.reserve >= 0.0 && self.reserve_target >= 0.0) {
            return self.fail("the opening cash, reserve or reserve target is invalid");
        }
        let mut replaced = vec![false; n];
        let mut exercises: Vec<&Exercise> = self.exercises.iter().collect();
        exercises.sort_by_key(|e| e.t);
        for e in exercises {
            tranche(e.by)?;
            let by = &self.tranches[e.by].name;
            if self.payments.binary_search(&e.t).is_err() {
                return self.fail(format!("an exercise at {} is not on a payment date", e.t));
            }
            let Some(right) = self.rights.iter().find(|r| r.kind == e.kind && r.holder == e.by) else {
                return self.fail(format!("{by} holds no right to {:?}", e.kind));
            };
            if e.t < right.from {
                return self.fail(format!("{:?} at {} is before the end of the non-call period, {}", e.kind, e.t, right.from));
            }
            if e.share < right.majority {
                return self.fail(format!("{:?} needs {} of {by} to consent, and has {}", e.kind, right.majority, e.share));
            }
            if e.kind == RightKind::Call {
                continue;
            }
            if e.redeem.is_empty() || e.redeem.len() != e.replacements.len() {
                return self.fail("a refinancing redeems classes one for one with their replacements");
            }
            for (&o, &r) in e.redeem.iter().zip(&e.replacements) {
                tranche(o)?;
                tranche(r)?;
                if self.tranches[o].claim != Claim::Debt || self.tranches[r].claim != Claim::Debt {
                    return self.fail("a refinancing redeems and issues debt");
                }
                if self.tranches[r].issued || replaced[r] {
                    return self.fail(format!("{} is outstanding already, so it cannot be issued as a replacement", self.tranches[r].name));
                }
                replaced[r] = true;
            }
        }
        Ok(())
    }

    fn reads_all(&self) -> Vec<String> {
        let mut r: BTreeSet<String> = self.collateral.iter().flat_map(|c| c.reads()).collect();
        r.extend(self.reads.iter().cloned());
        for h in &self.haircuts {
            r.insert(h.when.0.clone());
            for c in &h.carry {
                if let Carry::Column { series, .. } = c {
                    r.insert(series.clone());
                }
            }
        }
        for t in &self.triggers {
            if let Metric::Series(s, _) = &t.metric {
                r.insert(s.clone());
            }
        }
        for tr in &self.tranches {
            if let Coupon::Floating { index, .. } = &tr.coupon {
                r.insert(index.0.clone());
            }
        }
        r.into_iter().collect()
    }

    fn params(&self) -> u64 {
        let mut h = Hash64::new();
        h.str(&format!(
            "{:?}",
            (
                &self.opening,
                self.cash,
                self.reserve,
                self.reserve_target,
                &self.tranches,
                &self.fees,
                &self.haircuts,
                &self.tests,
                &self.triggers,
                &self.interest,
                &self.principal,
                &self.losses,
            )
        ));
        h.str(&format!(
            "{:?}",
            (&self.payments, self.reinvest_until, self.maturity, &self.rights, &self.exercises, self.missing, self.accounts, self.rounding)
        ));
        for c in &self.collateral {
            h.u64(u64::from(c.entity().0));
            h.str(c.kind());
            h.u64(c.params());
        }
        for m in &self.manager {
            h.str(&m.name);
            h.str(&m.decide.name());
            h.u64(m.decide.params());
            for t in &m.schedule {
                h.i64(t.0);
            }
            h.u64(m.lag as u64);
        }
        h.str(&self.fill.name());
        h.u64(u64::from(self.entity.0));
        h.finish()
    }

    fn provenance(&self, what: &str) -> Provenance {
        Provenance {
            knowledge: KnowledgeBasis::Derived,
            producer: Producer::Book { name: self.name.clone() },
            describes: Some(format!("{}.{what}", self.name)),
            intervention: None,
        }
    }

    fn written(&self, cal: &Calendar, s: &State) -> Result<Vec<Series>> {
        let times = cal.times().to_vec();
        let n = self.tranches.len();
        let name = |w: &str| format!("{}.{w}", self.name);
        let cols = |names: &[&str], values: Vec<Vec<f64>>| -> Vec<Column> { names.iter().zip(values).map(|(c, v)| Column::new(*c, Unit::Number, v)).collect() };

        let mut fe = Vec::new();
        let mut ft = Vec::new();
        let mut fv = vec![Vec::new(); 5];
        for (t, j, v) in &s.out.flows {
            fe.push(self.tranches[*j].entity);
            ft.push(*t);
            for (c, x) in v.iter().enumerate() {
                fv[c].push(*x);
            }
        }
        let flows = Series::rows(name("flows"), fe, ft.clone(), ft, cols(&["interest", "principal_face", "writedown_face", "writeup_face", "pik_face"], fv))?
            .with_provenance(self.provenance("flows"));

        let mut mv = vec![vec![f64::NAN; times.len() * n]; 6];
        for (r, row) in s.out.marks.iter().enumerate() {
            for (c, x) in row.iter().enumerate() {
                mv[c][r] = *x;
            }
        }
        let entities: Vec<EntityId> = self.tranches.iter().map(|t| t.entity).collect();
        let marks = Series::grid(
            name("marks"),
            entities,
            times.clone(),
            times.clone(),
            cols(&["clean", "accrued", "factor", "balance", "deferred", "deficiency"], mv),
        )?
        .with_provenance(self.provenance("marks"));

        let recipients: Vec<EntityId> = self.tranches.iter().map(|t| t.entity).chain(self.fees.iter().map(|f| f.entity)).chain([self.entity]).collect();
        let mut pe = Vec::new();
        let mut pt = Vec::new();
        let mut pv = vec![Vec::new(); 3];
        for (t, r, v) in &s.out.paid {
            pe.push(recipients[*r]);
            pt.push(*t);
            for (c, x) in v.iter().enumerate() {
                pv[c].push(*x);
            }
        }
        let paid = Series::rows(name("distributions"), pe, pt.clone(), pt, cols(&["interest", "principal", "fee"], pv))?
            .with_provenance(self.provenance("distributions"));

        let mut tt: Vec<Ts> = s.out.tests.iter().map(|(t, _, _)| *t).collect();
        tt.dedup();
        let te: Vec<EntityId> = self.tests.iter().map(|t| t.entity).chain(self.triggers.iter().map(|t| t.entity)).collect();
        let mut tv = vec![vec![f64::NAN; tt.len() * te.len()]; 6];
        for (t, i, v) in &s.out.tests {
            let ti = tt.binary_search(t).expect("a recorded time");
            for (c, x) in v.iter().enumerate() {
                tv[c][ti * te.len() + i] = *x;
            }
        }
        let mut tcols = cols(&["value", "threshold", "passed", "cure", "after", "passed_after"], tv);
        tcols[2].unit = Unit::Flag;
        tcols[5].unit = Unit::Flag;
        let tests = Series::grid(name("tests"), te, tt.clone(), tt, tcols)?.with_provenance(self.provenance("tests"));

        let mut av = vec![vec![f64::NAN; times.len() * 3]];
        for (k, row) in s.out.accounts.iter().enumerate() {
            av[0][k * 3..k * 3 + 3].copy_from_slice(row);
        }
        let accounts = Series::grid(name("accounts"), self.accounts.to_vec(), times.clone(), times.clone(), cols(&["balance"], std::mem::take(&mut av)))?
            .with_provenance(self.provenance("accounts"));

        let mut cv = vec![vec![f64::NAN; times.len()]; 7];
        for (k, row) in s.out.collateral.iter().enumerate() {
            for (c, x) in row.iter().enumerate() {
                cv[c][k] = *x;
            }
        }
        let collateral = Series::grid(
            name("collateral"),
            vec![self.entity],
            times.clone(),
            times,
            cols(&["par", "value", "interest", "principal", "losses", "nav", "reinvesting"], cv),
        )?
        .with_provenance(self.provenance("collateral"));

        let ce: Vec<EntityId> = s.out.claims.iter().map(|(_, new, _, _)| self.tranches[*new].entity).collect();
        let ct: Vec<Ts> = s.out.claims.iter().map(|(t, ..)| *t).collect();
        let replaces = s.out.claims.iter().map(|(_, _, old, _)| self.tranches[*old].entity.as_value()).collect();
        let balance = s.out.claims.iter().map(|(.., b)| *b).collect();
        let claims = Series::rows(
            name("claims"),
            ce,
            ct.clone(),
            ct,
            vec![Column::new("replaces", Unit::Ref, replaces), Column::new("balance", Unit::Number, balance)],
        )?
        .with_provenance(self.provenance("claims"));
        Ok(vec![flows, marks, paid, tests, accounts, collateral, claims])
    }

    /// Runs the deal over `cal`: the collateral book with its waterfall. Returns the waterfall's
    /// final state.
    fn run(&self, ws: &Workspace, cal: &Calendar) -> Result<State> {
        let (records, _) = UnitRecords::compute(&self.collateral, ws, cal, self.missing)?;
        let mut ledger = self.book(ws, &records)?;
        while ledger.step()? {}
        ledger
            .distributor_state()
            .and_then(|s| s.state_any().downcast_ref::<State>())
            .cloned()
            .ok_or_else(|| Error::Invalid(format!("deal {}: the waterfall left no state", self.name)))
    }

    /// The collateral book on `records` (the collateral's unit records over the deal's calendar),
    /// with the waterfall as its distributor: what an evaluation steps. Its checkpoints hold the
    /// waterfall's state.
    ///
    /// # Errors
    /// Inconsistent terms (see [`Deal::validate`]), a payment date off the calendar, or the
    /// ledger's own errors.
    pub fn book<'a>(&'a self, ws: &'a Workspace, records: &'a UnitRecords) -> Result<Ledger<'a>> {
        self.validate()?;
        for p in &self.payments {
            if records.calendar().index(*p).is_none() {
                return self.fail(format!("payment date {p} is not on the calendar"));
            }
        }
        let waterfall = Waterfall { deal: Arc::new(self.clone()) };
        let cfg = LedgerConfig {
            initial_cash: self.cash,
            routing: Routing::Collections,
            opening: self.opening.clone(),
            distributor: Some(DistributorRef(Arc::new(waterfall))),
            ..LedgerConfig::default()
        };
        let policies = self
            .manager
            .iter()
            .map(|m| Policy {
                name: m.name.clone(),
                decide: m.decide.as_ref(),
                schedule: m.schedule.clone(),
                scope: None,
                priority: 0,
                lag: m.lag,
                time_in_force: None,
            })
            .collect();
        Ledger::build(ws, records, policies, ExecRef::Fill(self.fill.as_ref()), cfg)
    }
}

impl BatchModel for Deal {
    fn spec(&self) -> ModelSpec {
        let mut spec = ModelSpec::new(&self.name, Shape::Batch);
        spec.writes = self.outputs();
        spec.reads = self.reads_all();
        spec.params = self.params();
        spec.parallel = self.parallel;
        spec.describes = Some(format!("{}.deal", self.name));
        spec
    }

    fn evaluate(&self, ws: &Workspace, cal: &Calendar) -> Result<Vec<Series>> {
        let s = self.run(ws, cal)?;
        self.written(cal, &s)
    }

    fn supports(&self) -> Vec<&'static str> {
        vec![
            "collateral book",
            "manager policies",
            "fees",
            "fixed and floating coupons",
            "deferral",
            "pik",
            "arrears and events of default",
            "oc test",
            "ic test",
            "adjusted collateral haircuts",
            "excess concentration",
            "cure by paydown",
            "cure by reinvestment",
            "reserve",
            "deficiency ledger",
            "write-down and write-up",
            "sequential",
            "pro rata",
            "pac schedule",
            "interest only",
            "principal only",
            "triggers with lookback",
            "curable triggers",
            "early amortization",
            "reinvestment period",
            "call",
            "refinancing",
            "reset",
            "legal final maturity",
        ]
    }

    /// A manager deciding other than as recorded makes the deal a counterfactual: its pool, tests
    /// and distributions are its own, not the real deal's.
    fn intervention(&self, ws: &Workspace) -> Option<Intervention> {
        self.manager.iter().find(|m| m.decide.intervenes(ws)).map(|m| Intervention::Counterfactual(format!("{}'s manager ({})", self.name, m.decide.name())))
    }
}

// ------------------------------------------------------------------------------------ waterfall

const INTEREST: usize = 0;
const PRINCIPAL: usize = 1;
const WRITEDOWN: usize = 2;
const WRITEUP: usize = 3;
const PIK: usize = 4;

/// The deal's state: what its terms remember from one payment date to the next.
#[derive(Clone, Default)]
struct State {
    started: bool,
    ended: bool,
    balance: Vec<f64>,
    deferred: Vec<f64>,
    deficiency: Vec<f64>,
    written: Vec<f64>,
    issued: Vec<bool>,
    ever: Vec<bool>,
    successor: Vec<Option<usize>>,
    rate: Vec<f64>,
    due: Vec<f64>,
    fee_due: Vec<f64>,
    fee_carried: Vec<f64>,
    reserve: f64,
    tripped: Vec<bool>,
    observed: Vec<Vec<f64>>,
    start: Ts,
    par_start: f64,
    original_par: f64,
    period: Collected,
    period_loss: f64,
    cum_loss: f64,
    reinvest_until: Option<Ts>,
    default_event: bool,
    interest_start: f64,
    fees_paid: f64,
    pending: Vec<[f64; 5]>,
    paid: Vec<[f64; 3]>,
    out: Out,
}

#[derive(Clone, Default)]
struct Out {
    flows: Vec<(Ts, usize, [f64; 5])>,
    paid: Vec<(Ts, usize, [f64; 3])>,
    marks: Vec<[f64; 6]>,
    tests: Vec<(Ts, usize, [f64; 6])>,
    accounts: Vec<[f64; 3]>,
    collateral: Vec<[f64; 7]>,
    claims: Vec<(Ts, usize, usize, f64)>,
}

impl State {
    /// The claim outstanding in place of `j`: itself, or its replacement once redeemed.
    fn live(&self, mut j: usize) -> usize {
        while !self.issued[j] {
            match self.successor[j] {
                Some(n) if n != j => j = n,
                _ => break,
            }
        }
        j
    }

    /// The outstanding claims in place of `js`, once each, in order.
    fn outstanding(&self, js: &[usize]) -> Vec<usize> {
        let mut out: Vec<usize> = Vec::with_capacity(js.len());
        for &j in js {
            let j = self.live(j);
            if self.issued[j] && !out.contains(&j) {
                out.push(j);
            }
        }
        out
    }

    fn reference(&self, of: Notional, par: f64) -> f64 {
        match of {
            Notional::Pool => par,
            Notional::Tranche(i) => self.balance[self.live(i)],
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum Acct {
    Interest,
    Principal,
}

fn avail(d: &Distribution<'_>, a: Acct) -> f64 {
    match a {
        Acct::Interest => d.interest(),
        Acct::Principal => d.principal(),
    }
    .max(0.0)
}

fn ratio(num: f64, den: f64) -> f64 {
    if den > TINY { num / den } else { f64::INFINITY }
}

/// A deal's priority of payments over its collateral book: the book's distributor.
pub struct Waterfall {
    deal: Arc<Deal>,
}

impl Distributor for Waterfall {
    fn name(&self) -> String {
        format!("waterfall({})", self.deal.name)
    }

    fn start(&self) -> Box<dyn DistributorState> {
        Box::new(State::default())
    }

    fn distribute(&self, d: &mut Distribution<'_>, state: &mut dyn DistributorState) -> Result<()> {
        let s = state
            .state_any_mut()
            .downcast_mut::<State>()
            .ok_or_else(|| Error::Invalid(format!("deal {}: the waterfall's state is not its own", self.deal.name)))?;
        if !s.started {
            self.open(d, s)?;
        }
        if !s.ended {
            let c = d.collected;
            s.period.interest += c.interest;
            s.period.principal += c.principal;
            s.period.par_drop += c.par_drop;
            s.period_loss += c.loss();
            s.cum_loss += c.loss();
            if self.deal.payments.binary_search(&d.t).is_ok() {
                self.pay(d, s)?;
            }
        }
        d.publish("reinvesting", if self.reinvesting(s, d.t) { 1.0 } else { 0.0 });
        d.publish("event_of_default", if s.default_event { 1.0 } else { 0.0 });
        self.record(d, s);
        Ok(())
    }
}

impl Waterfall {
    /// An amount due, in whole units by the deal's rounding mode.
    fn due(&self, x: f64) -> f64 {
        self.deal.rounding.map_or(x, |r| r.round(x))
    }

    /// The smallest whole amount not below `x`: what a cure must pay.
    fn up(&self, x: f64) -> f64 {
        self.deal.rounding.map_or(x, |r| r.up(x))
    }

    /// Pays up to `x` out of an account: in whole units where the deal rounds, never a fraction
    /// of a unit, which stays in its account.
    fn take(&self, d: &mut Distribution<'_>, a: Acct, x: f64) -> f64 {
        let x = self.deal.rounding.map_or(x, |r| r.down(x.min(avail(d, a))));
        match a {
            Acct::Interest => d.pay_interest(x),
            Acct::Principal => d.pay_principal(x),
        }
    }

    /// Moves up to `x` of interest into the principal account, in whole units where the deal
    /// rounds.
    fn to_principal(&self, d: &mut Distribution<'_>, x: f64) -> f64 {
        let x = self.deal.rounding.map_or(x, |r| r.down(x.min(d.interest().max(0.0))));
        d.interest_to_principal(x)
    }

    /// Moves up to `x` of principal into the interest account, in whole units where the deal
    /// rounds.
    fn to_interest(&self, d: &mut Distribution<'_>, x: f64) -> f64 {
        let x = self.deal.rounding.map_or(x, |r| r.down(x.min(d.principal().max(0.0))));
        d.principal_to_interest(x)
    }

    /// Splits `total` across `claims` pro rata, none paid more than it is owed.
    fn split(&self, total: f64, claims: &[f64]) -> Vec<f64> {
        self.apportion(total, claims, true)
    }

    /// Shares `total` out by `weights` (the equity's faces), with no claim to cap a share.
    fn share_out(&self, total: f64, weights: &[f64]) -> Vec<f64> {
        self.apportion(total, weights, false)
    }

    /// Pro rata: exactly where the deal does not round; in whole units where it does, the units
    /// that rounding each share down leaves go to the largest remainders, the earlier first among
    /// equals.
    fn apportion(&self, total: f64, claims: &[f64], capped: bool) -> Vec<f64> {
        let sum: f64 = claims.iter().sum();
        if sum <= 0.0 || total <= 0.0 {
            return vec![0.0; claims.len()];
        }
        let cap = |x: f64, c: f64| if capped { x.min(c) } else { x };
        let exact: Vec<f64> = claims.iter().map(|c| cap(total * c / sum, *c)).collect();
        let Some(r) = self.deal.rounding else { return exact };
        let mut shares: Vec<f64> = exact.iter().map(|x| r.down(*x)).collect();
        let left = r.down(total) - shares.iter().sum::<f64>();
        let mut units = (left / r.unit).round().max(0.0) as usize;
        let mut order: Vec<usize> = (0..claims.len()).collect();
        order.sort_by(|&a, &b| (exact[b] - shares[b]).total_cmp(&(exact[a] - shares[a])));
        for i in order {
            if units == 0 {
                break;
            }
            if !capped || shares[i] + r.unit <= claims[i] + 1e-9 {
                shares[i] += r.unit;
                units -= 1;
            }
        }
        shares
    }

    fn rate(&self, ws: &Workspace, j: usize, t: Ts) -> Result<f64> {
        let tr = &self.deal.tranches[j];
        Ok(match &tr.coupon {
            Coupon::None => 0.0,
            Coupon::Fixed(r) => *r,
            Coupon::Floating { index: (s, c), spread, floor } => {
                let series = ws.series(s)?;
                let col = series.column(c)?;
                let x = series
                    .asof(col, EntityId::NONE, t, t)
                    .map(|h| h.value)
                    .filter(|v| v.is_finite())
                    .ok_or_else(|| Error::Missing(format!("deal {}: no {s}.{c} known at {t} for the coupon of {}", self.deal.name, tr.name)))?;
                floor.map_or(x + spread, |f| (x + spread).max(f))
            }
        })
    }

    fn open(&self, d: &Distribution<'_>, s: &mut State) -> Result<()> {
        let deal = &*self.deal;
        let n = deal.tranches.len();
        s.issued = deal.tranches.iter().map(|tr| tr.issued).collect();
        s.ever.clone_from(&s.issued);
        s.balance = deal.tranches.iter().map(|tr| if tr.issued { tr.balance.unwrap_or(tr.face) } else { 0.0 }).collect();
        s.deferred = vec![0.0; n];
        s.deficiency = vec![0.0; n];
        s.written = vec![0.0; n];
        s.due = vec![0.0; n];
        s.successor = vec![None; n];
        for e in &deal.exercises {
            for (o, r) in e.redeem.iter().zip(&e.replacements) {
                s.successor[*o] = Some(*r);
            }
        }
        s.pending = vec![[0.0; 5]; n];
        s.paid = vec![[0.0; 3]; n + deal.fees.len() + 1];
        s.fee_due = vec![0.0; deal.fees.len()];
        s.fee_carried = vec![0.0; deal.fees.len()];
        s.reserve = deal.reserve;
        s.tripped = vec![false; deal.triggers.len()];
        s.observed = vec![Vec::new(); deal.triggers.len()];
        s.start = d.t;
        s.original_par = d.par();
        s.par_start = s.original_par;
        s.reinvest_until = deal.reinvest_until;
        s.rate = vec![0.0; n];
        for j in 0..n {
            if s.issued[j] {
                s.rate[j] = self.rate(d.view.ws, j, d.t)?;
            }
            if let Claim::InterestOnly { of, ratio } = deal.tranches[j].claim
                && s.issued[j]
                && deal.tranches[j].balance.is_none()
            {
                s.balance[j] = ratio * s.reference(of, s.original_par);
            }
        }
        s.started = true;
        Ok(())
    }

    fn reinvesting(&self, s: &State, t: Ts) -> bool {
        !s.ended
            && s.reinvest_until.is_some_and(|u| t < u)
            && !self.deal.triggers.iter().zip(&s.tripped).any(|(tr, &x)| x && tr.effect == Effect::EndReinvestment)
    }

    fn holds(&self, when: When, s: &State, t: Ts) -> bool {
        match when {
            When::Always => true,
            When::Reinvesting => self.reinvesting(s, t),
            When::Amortising => !self.reinvesting(s, t),
            When::Tripped(i) => s.tripped[i],
            When::NotTripped(i) => !s.tripped[i],
        }
    }

    /// One payment date.
    fn pay(&self, d: &mut Distribution<'_>, s: &mut State) -> Result<()> {
        let deal = &*self.deal;
        let t = d.t;
        let n = deal.tranches.len();
        let days = (t.0 - s.start.0) as f64 / Ts::DAY as f64;
        for j in 0..n {
            s.due[j] = 0.0;
            let tr = &deal.tranches[j];
            if !s.issued[j] || tr.claim == Claim::Equity {
                continue;
            }
            let yf = days / tr.basis;
            if let Shortfall::Defer { compound: true } = tr.shortfall {
                s.deferred[j] += self.due(s.deferred[j] * s.rate[j] * yf);
            }
            s.due[j] = self.due(s.balance[j] * s.rate[j] * yf);
        }
        for (f, fee) in deal.fees.iter().enumerate() {
            s.fee_due[f] = self.due(fee.rate * s.par_start * days / 360.0 + fee.fixed) + s.fee_carried[f];
        }
        s.interest_start = d.interest();
        s.fees_paid = 0.0;
        self.allocate_losses(s);
        self.observe(d, s, days)?;
        let exercise = deal.exercises.iter().find(|e| e.t == t);
        if deal.maturity == Some(t) || exercise.is_some_and(|e| e.kind == RightKind::Call) {
            self.wind_up(d, s);
        } else {
            if let Some(e) = exercise {
                self.expenses(d, s, e);
            }
            for step in &deal.interest {
                if self.holds(step.when, s, t) {
                    self.apply(d, s, &step.action, Acct::Interest, days)?;
                }
            }
            for step in &deal.principal {
                if self.holds(step.when, s, t) {
                    self.apply(d, s, &step.action, Acct::Principal, days)?;
                }
            }
            if let Some(e) = exercise {
                self.redeem(d, s, e)?;
            }
            self.shortfalls(d, s);
            self.notionals(d, s);
        }
        for j in 0..n {
            let p = std::mem::take(&mut s.pending[j]);
            if p.iter().any(|x| *x != 0.0) {
                let f = deal.tranches[j].face;
                s.out.flows.push((t, j, p.map(|x| x / f)));
            }
        }
        for r in 0..s.paid.len() {
            let p = std::mem::take(&mut s.paid[r]);
            if p.iter().any(|x| *x != 0.0) {
                s.out.paid.push((t, r, p));
            }
        }
        s.start = t;
        s.par_start = d.par();
        s.period = Collected::default();
        if !s.ended {
            for j in 0..n {
                if s.issued[j] {
                    s.rate[j] = self.rate(d.view.ws, j, t)?;
                }
            }
        }
        Ok(())
    }

    fn allocate_losses(&self, s: &mut State) {
        let loss = std::mem::take(&mut s.period_loss);
        match &self.deal.losses {
            Losses::Kept => {}
            Losses::WriteDown { order, write_up } => {
                if loss > TINY {
                    let mut left = loss;
                    for &j in order {
                        let j = s.live(j);
                        if !s.issued[j] || left <= TINY {
                            continue;
                        }
                        let w = left.min(s.balance[j]).max(0.0);
                        s.balance[j] -= w;
                        s.written[j] += w;
                        s.pending[j][WRITEDOWN] += w;
                        left -= w;
                    }
                } else if loss < -TINY && *write_up {
                    let mut left = -loss;
                    for &j in order.iter().rev() {
                        let j = s.live(j);
                        let u = left.min(s.written[j]);
                        s.balance[j] += u;
                        s.written[j] -= u;
                        s.pending[j][WRITEUP] += u;
                        left -= u;
                    }
                }
            }
            Losses::Ledger(order) => {
                if loss > TINY {
                    let mut left = loss;
                    for &j in order {
                        let j = s.live(j);
                        if !s.issued[j] {
                            continue;
                        }
                        let x = left.min((s.balance[j] - s.deficiency[j]).max(0.0));
                        s.deficiency[j] += x;
                        left -= x;
                    }
                } else if loss < -TINY {
                    let mut left = -loss;
                    for &j in order.iter().rev() {
                        let j = s.live(j);
                        let x = left.min(s.deficiency[j]);
                        s.deficiency[j] -= x;
                        left -= x;
                    }
                }
            }
        }
    }

    /// The triggers, at the payment date's observation point (before the waterfall).
    fn observe(&self, d: &Distribution<'_>, s: &mut State, days: f64) -> Result<()> {
        let deal = &*self.deal;
        for (i, tr) in deal.triggers.iter().enumerate() {
            let v = match &tr.metric {
                Metric::CumulativeLoss => s.cum_loss / s.original_par,
                Metric::PoolFactor => d.par() / s.original_par,
                Metric::PaymentRate => s.period.principal / s.par_start,
                Metric::ExcessSpread => {
                    let fees: f64 = s.fee_due.iter().zip(&s.fee_carried).map(|(a, c)| a - c).sum();
                    let owed = fees + s.due.iter().sum::<f64>();
                    (s.period.interest - owed) / s.par_start * 360.0 / days
                }
                Metric::Test(k) => {
                    let (num, den) = self.coverage(d, s, &deal.tests[*k], days)?;
                    ratio(num, den)
                }
                Metric::Series(name, col) => {
                    let series = d.view.series(name)?;
                    let c = series.column(col)?;
                    d.view
                        .asof(series, c, EntityId::NONE)
                        .map(|h| h.value)
                        .filter(|x| x.is_finite())
                        .ok_or_else(|| Error::Missing(format!("deal {}: trigger {} has no {name}.{col} known at {}", deal.name, tr.name, d.t)))?
                }
            };
            if v.is_finite() {
                s.observed[i].push(v);
            }
            let w = tr.lookback.max(1);
            let seen = &s.observed[i];
            let tail = &seen[seen.len().saturating_sub(w)..];
            let avg = if tail.is_empty() { f64::NAN } else { tail.iter().sum::<f64>() / tail.len() as f64 };
            if avg.is_finite() {
                let breach = tr.above.is_some_and(|a| avg > a) || tr.below.is_some_and(|b| avg < b);
                s.tripped[i] = if tr.curable { breach } else { s.tripped[i] || breach };
            }
            let threshold = tr.above.or(tr.below).unwrap_or(f64::NAN);
            let ok = if s.tripped[i] { 0.0 } else { 1.0 };
            s.out.tests.push((d.t, deal.tests.len() + i, [avg, threshold, ok, 0.0, avg, ok]));
        }
        Ok(())
    }

    /// A test's numerator and denominator now.
    fn coverage(&self, d: &Distribution<'_>, s: &State, test: &Test, days: f64) -> Result<(f64, f64)> {
        let classes = s.outstanding(&test.classes);
        Ok(match test.kind {
            TestKind::Oc => {
                let num = self.adjusted_par(d)? + d.principal().max(0.0);
                (num, classes.iter().map(|&j| s.balance[j] + s.deferred[j]).sum())
            }
            TestKind::Ic => {
                let den = classes.iter().map(|&j| s.balance[j] * s.rate[j] * days / self.deal.tranches[j].basis).sum();
                (s.interest_start - s.fees_paid, den)
            }
        })
    }

    /// The collateral's par after the haircut rules (F01).
    fn adjusted_par(&self, d: &Distribution<'_>) -> Result<f64> {
        let deal = &*self.deal;
        let m = d.holdings.len();
        let par: Vec<f64> = (0..m).map(|p| d.par_of(p)).map(|x| if x.is_finite() { x } else { 0.0 }).collect();
        let mut adjusted: f64 = par.iter().sum();
        let mut taken = vec![false; m];
        for h in &deal.haircuts {
            let series = d.view.series(&h.when.0)?;
            let col = series.column(&h.when.1)?;
            let mut hits: Vec<(usize, f64)> = Vec::new();
            for p in 0..m {
                if taken[p] || par[p] <= 0.0 {
                    continue;
                }
                let e = d.records.entities()[d.holdings[p].0];
                let flag = d.view.asof(series, col, e).map_or(0.0, |x| x.value);
                if flag == 0.0 || !flag.is_finite() {
                    continue;
                }
                let mut frac = f64::INFINITY;
                for c in &h.carry {
                    let x = match c {
                        Carry::Market => d.value_of(p) / par[p],
                        Carry::Column { series, column, per } => d.view.value(series, column, e).map_or(f64::NAN, |v| v / per),
                        Carry::Fixed(f) => *f,
                    };
                    if !x.is_finite() {
                        return Err(Error::Missing(format!(
                            "deal {}: haircut {} has no carrying value for {} at {}",
                            deal.name,
                            h.name,
                            d.view.ws.entities.name(e),
                            d.t
                        )));
                    }
                    frac = frac.min(x);
                }
                hits.push((p, frac.min(1.0)));
            }
            let base: f64 = (0..m).filter(|&p| !taken[p]).map(|p| par[p]).sum();
            match h.excess_over {
                None => {
                    for &(p, f) in &hits {
                        adjusted -= par[p] * (1.0 - f);
                    }
                }
                Some(limit) => {
                    let mut excess = hits.iter().map(|(p, _)| par[*p]).sum::<f64>() - limit * base;
                    hits.sort_by(|a, b| a.1.total_cmp(&b.1));
                    for &(p, f) in &hits {
                        if excess <= 0.0 {
                            break;
                        }
                        let part = excess.min(par[p]);
                        adjusted -= part * (1.0 - f);
                        excess -= part;
                    }
                }
            }
            for &(p, _) in &hits {
                taken[p] = true;
            }
        }
        Ok(adjusted)
    }

    fn apply(&self, d: &mut Distribution<'_>, s: &mut State, action: &Action, acct: Acct, days: f64) -> Result<()> {
        let deal = &*self.deal;
        let n = deal.tranches.len();
        match action {
            Action::Fee(f) => {
                let x = self.take(d, acct, s.fee_due[*f]);
                s.fee_due[*f] -= x;
                s.fees_paid += x;
                s.paid[n + *f][2] += x;
            }
            Action::Interest { tranches, reserve } => {
                let js = s.outstanding(tranches);
                let claims: Vec<f64> = js.iter().map(|&j| s.deferred[j] + s.due[j]).collect();
                let total: f64 = claims.iter().sum();
                if total <= TINY {
                    return Ok(());
                }
                let mut paid = self.take(d, acct, total);
                if *reserve && paid < total {
                    let draw = (total - paid).min(s.reserve);
                    s.reserve -= draw;
                    paid += draw;
                }
                let shares = self.split(paid, &claims);
                for (&j, &x) in js.iter().zip(&shares) {
                    let def = x.min(s.deferred[j]);
                    s.deferred[j] -= def;
                    s.due[j] = (s.due[j] - (x - def)).max(0.0);
                    s.pending[j][INTEREST] += x;
                    s.paid[j][0] += x;
                }
            }
            Action::Principal { tranches, pro_rata, schedule } => {
                let js = s.outstanding(tranches);
                let claims: Vec<f64> = js
                    .iter()
                    .map(|&j| {
                        let floor = if *schedule { deal.tranches[j].scheduled(d.t).unwrap_or(s.balance[j]) } else { 0.0 };
                        (s.balance[j] - floor).max(0.0)
                    })
                    .collect();
                let total: f64 = claims.iter().sum();
                let amount = avail(d, acct).min(total);
                if amount <= TINY {
                    return Ok(());
                }
                let mut left = amount;
                let wants = if *pro_rata { self.split(amount, &claims) } else { claims.clone() };
                for (&j, &want) in js.iter().zip(&wants) {
                    let x = self.take(d, acct, want.min(left));
                    s.balance[j] -= x;
                    s.pending[j][PRINCIPAL] += x;
                    s.paid[j][1] += x;
                    left -= x;
                }
            }
            Action::Test(i) => self.test(d, s, *i, acct, days)?,
            Action::Reserve => {
                let x = self.take(d, acct, (deal.reserve_target - s.reserve).max(0.0));
                s.reserve += x;
            }
            Action::Deficiency(order) => {
                for &j in order {
                    let j = s.live(j);
                    s.deficiency[j] -= self.to_principal(d, s.deficiency[j]);
                }
            }
            Action::ToPrincipal => {
                self.to_principal(d, f64::INFINITY);
            }
            Action::ToInterest => {
                self.to_interest(d, f64::INFINITY);
            }
            Action::Residual(j) => {
                let j = s.live(*j);
                let x = self.take(d, acct, f64::INFINITY);
                s.pending[j][INTEREST] += x;
                s.paid[j][0] += x;
            }
        }
        Ok(())
    }

    fn test(&self, d: &mut Distribution<'_>, s: &mut State, i: usize, acct: Acct, days: f64) -> Result<()> {
        let deal = &*self.deal;
        let test = &deal.tests[i];
        let thr = test.threshold;
        let (num, den) = self.coverage(d, s, test, days)?;
        let before = ratio(num, den);
        let passed = before >= thr * (1.0 - 1e-12);
        let mut cure = 0.0;
        if !passed {
            match (&test.cure, test.kind) {
                (Cure::None, _) => {}
                (Cure::Reinvest, _) => cure = self.to_principal(d, self.up(thr * den - num)),
                (Cure::Paydown(order), TestKind::Oc) => {
                    // Interest paid as principal lowers the debt only: interest was never part of
                    // the numerator. Principal cash paid out lowers both.
                    let need = match acct {
                        Acct::Interest => den - num / thr,
                        Acct::Principal if thr > 1.0 => (thr * den - num) / (thr - 1.0),
                        Acct::Principal => 0.0,
                    };
                    cure = self.pay_down(d, s, order, acct, self.up(need));
                }
                (Cure::Paydown(order), TestKind::Ic) => {
                    let classes = s.outstanding(&test.classes);
                    let mut den = den;
                    for &j in order {
                        let j = s.live(j);
                        let per = s.rate[j] * days / deal.tranches[j].basis;
                        let gap = den - num / thr;
                        if gap <= TINY {
                            break;
                        }
                        if !classes.contains(&j) || per <= 0.0 {
                            continue;
                        }
                        let x = self.pay_down(d, s, &[j], acct, self.up(gap / per));
                        den -= x * per;
                        cure += x;
                    }
                }
            }
        }
        let (num, den) = self.coverage(d, s, test, days)?;
        let after = ratio(num, den);
        let flag = |x: bool| if x { 1.0 } else { 0.0 };
        s.out.tests.push((d.t, i, [before, thr, flag(passed), cure, after, flag(after >= thr * (1.0 - 1e-12))]));
        Ok(())
    }

    fn pay_down(&self, d: &mut Distribution<'_>, s: &mut State, order: &[usize], acct: Acct, need: f64) -> f64 {
        let mut left = need.max(0.0);
        let mut paid = 0.0;
        for &j in order {
            let j = s.live(j);
            if left <= TINY || !s.issued[j] || self.deal.tranches[j].claim != Claim::Debt {
                continue;
            }
            let x = self.take(d, acct, left.min(s.balance[j]));
            s.balance[j] -= x;
            s.pending[j][PRINCIPAL] += x;
            s.paid[j][1] += x;
            left -= x;
            paid += x;
        }
        paid
    }

    /// A call or the legal final maturity: sell the collateral at its value, pay by seniority,
    /// release the reserve to the equity, and write off what is unpaid.
    fn wind_up(&self, d: &mut Distribution<'_>, s: &mut State) {
        let deal = &*self.deal;
        let n = deal.tranches.len();
        d.liquidate();
        self.to_interest(d, f64::INFINITY);
        for f in 0..deal.fees.len() {
            let x = self.take(d, Acct::Interest, s.fee_due[f]);
            s.fee_due[f] -= x;
            s.paid[n + f][2] += x;
        }
        for j in 0..n {
            if s.issued[j] && deal.tranches[j].claim != Claim::Equity {
                let x = self.take(d, Acct::Interest, s.deferred[j] + s.due[j]);
                s.pending[j][INTEREST] += x;
                s.paid[j][0] += x;
            }
        }
        for j in 0..n {
            if s.issued[j] && deal.tranches[j].claim == Claim::Debt {
                let x = self.take(d, Acct::Interest, s.balance[j]);
                s.balance[j] -= x;
                s.pending[j][PRINCIPAL] += x;
                s.paid[j][1] += x;
            }
        }
        let equity: Vec<usize> = (0..n).filter(|&j| s.issued[j] && deal.tranches[j].claim == Claim::Equity).collect();
        let faces: f64 = equity.iter().map(|&j| s.balance[j]).sum();
        let rest = self.deal.rounding.map_or(d.interest().max(0.0), |r| r.down(d.interest().max(0.0)));
        let reserve = std::mem::take(&mut s.reserve);
        let weights: Vec<f64> = equity.iter().map(|&j| if faces > 0.0 { s.balance[j] } else { 1.0 }).collect();
        let (cash, kept) = (self.share_out(rest, &weights), self.share_out(reserve, &weights));
        for (k, &j) in equity.iter().enumerate() {
            let x = self.take(d, Acct::Interest, cash[k]) + kept[k];
            s.pending[j][INTEREST] += x;
            s.paid[j][0] += x;
        }
        for j in 0..n {
            if s.issued[j] && s.balance[j] > 0.0 {
                s.pending[j][WRITEDOWN] += s.balance[j];
                s.balance[j] = 0.0;
            }
            s.deferred[j] = 0.0;
            s.due[j] = 0.0;
            s.deficiency[j] = 0.0;
        }
        s.ended = true;
    }

    fn expenses(&self, d: &mut Distribution<'_>, s: &mut State, e: &Exercise) {
        let from_interest = self.take(d, Acct::Interest, e.expenses);
        let rest = e.expenses - from_interest;
        let slot = self.deal.tranches.len() + self.deal.fees.len();
        s.paid[slot][2] += e.expenses;
        if rest > TINY {
            let j = s.live(e.by);
            s.pending[j][INTEREST] -= rest;
            s.paid[j][0] -= rest;
        }
    }

    /// A refinancing's redemptions: each old claim is paid once, at par plus anything unpaid,
    /// from its replacement's proceeds, and the replacement is issued with its own terms.
    fn redeem(&self, d: &Distribution<'_>, s: &mut State, e: &Exercise) -> Result<()> {
        let deal = &*self.deal;
        for (&old, &new) in e.redeem.iter().zip(&e.replacements) {
            if !s.issued[old] {
                return Err(Error::Invalid(format!("deal {}: {} is not outstanding at {}", deal.name, deal.tranches[old].name, d.t)));
            }
            let b = s.balance[old];
            let unpaid = s.deferred[old] + s.due[old];
            s.pending[old][PRINCIPAL] += b;
            s.pending[old][INTEREST] += unpaid;
            s.paid[old][1] += b;
            s.paid[old][0] += unpaid;
            s.balance[old] = 0.0;
            s.deferred[old] = 0.0;
            s.due[old] = 0.0;
            s.issued[old] = false;
            s.issued[new] = true;
            s.ever[new] = true;
            s.balance[new] = b + unpaid;
            s.rate[new] = self.rate(d.view.ws, new, d.t)?;
            s.out.claims.push((d.t, new, old, b + unpaid));
        }
        if e.kind == RightKind::Reset
            && let Some(u) = e.reinvest_until
        {
            s.reinvest_until = Some(u);
        }
        Ok(())
    }

    /// Interest still unpaid after both priorities, by each tranche's terms (F02).
    fn shortfalls(&self, d: &mut Distribution<'_>, s: &mut State) {
        let deal = &*self.deal;
        for (j, tr) in deal.tranches.iter().enumerate() {
            let u = std::mem::take(&mut s.due[j]);
            if u <= TINY {
                continue;
            }
            match tr.shortfall {
                Shortfall::Default => {
                    s.deferred[j] += u;
                    if !s.default_event {
                        s.default_event = true;
                        d.note(format!("deal {}: interest on {} unpaid at {}: an event of default", deal.name, tr.name, d.t));
                    }
                }
                Shortfall::Defer { .. } => s.deferred[j] += u,
                Shortfall::Capitalise => {
                    s.balance[j] += u;
                    s.pending[j][PIK] += u;
                }
                Shortfall::Extinguish => {}
            }
        }
        for (f, fee) in deal.fees.iter().enumerate() {
            s.fee_carried[f] = if fee.carry { s.fee_due[f] } else { 0.0 };
            s.fee_due[f] = 0.0;
        }
    }

    /// Interest-only notionals follow their references: the fall is written down (F14).
    fn notionals(&self, d: &Distribution<'_>, s: &mut State) {
        let par = d.par();
        for (j, tr) in self.deal.tranches.iter().enumerate() {
            if let Claim::InterestOnly { of, ratio } = tr.claim
                && s.issued[j]
            {
                let new = ratio * s.reference(of, par);
                let drop = s.balance[j] - new;
                if drop > 0.0 {
                    s.pending[j][WRITEDOWN] += drop;
                } else {
                    s.pending[j][WRITEUP] -= drop;
                }
                s.balance[j] = new;
            }
        }
    }

    /// The marks, accounts and pool at this time.
    fn record(&self, d: &Distribution<'_>, s: &mut State) {
        let deal = &*self.deal;
        let n = deal.tranches.len();
        let nav = d.nav - d.paid();
        let elapsed = (d.t.0 - s.start.0) as f64 / Ts::DAY as f64;
        let accrued: Vec<f64> = (0..n)
            .map(|j| {
                let tr = &deal.tranches[j];
                if !s.issued[j] || tr.claim == Claim::Equity { 0.0 } else { s.balance[j] * s.rate[j] * elapsed / tr.basis }
            })
            .collect();
        let debt: f64 = (0..n).filter(|&j| s.issued[j] && deal.tranches[j].claim == Claim::Debt).map(|j| s.balance[j] + s.deferred[j] + accrued[j]).sum();
        let book = nav + s.reserve - debt;
        let equity_face: f64 = (0..n).filter(|&j| s.issued[j] && deal.tranches[j].claim == Claim::Equity).map(|j| s.balance[j]).sum();
        for (j, tr) in deal.tranches.iter().enumerate() {
            if !s.ever[j] {
                s.out.marks.push([f64::NAN; 6]);
                continue;
            }
            let b = s.balance[j];
            let clean = match tr.claim {
                Claim::Debt => 100.0,
                Claim::InterestOnly { .. } => 0.0,
                Claim::Equity if equity_face > 0.0 => 100.0 * book.max(0.0) / equity_face,
                Claim::Equity => 0.0,
            };
            let acc = if b > TINY { 100.0 * (accrued[j] + s.deferred[j]) / b } else { 0.0 };
            s.out.marks.push([clean, acc, b / tr.face, b, s.deferred[j], s.deficiency[j]]);
        }
        s.out.accounts.push([d.interest(), d.principal(), s.reserve]);
        let reinvesting = if self.reinvesting(s, d.t) { 1.0 } else { 0.0 };
        s.out.collateral.push([d.par(), d.value(), d.collected.interest, d.collected.principal, s.cum_loss, nav, reinvesting]);
    }
}
