//! Contracts: per-unit state machines (axiom 2) whose flows carry roles (axiom 3).
//!
//! A unit's state (balance, accrued interest, status) is shared by every holder; a holding is
//! only a quantity. Each step returns the unit's value, its flows by role, and its P&L terms,
//! which add up exactly to the change in value plus the cash flows (invariant I1).

use crate::{
    Result,
    entity::EntityId,
    policy::{Diagnostics, MissingPolicy},
    series::Hit,
    time::Ts,
    workspace::Workspace,
};

/// The role of a flow or an accrual. Each role maps to a P&L term.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Role {
    /// Interest, coupons, dividends: income.
    Income = 0,
    /// Return or capitalisation of principal.
    Principal = 1,
    /// The performance of a level or a price (total-return legs, variation margin).
    Performance = 2,
    /// Funding paid or received (a TRS funding leg, repo interest): financing.
    Funding = 3,
    /// Fees.
    Fee = 4,
    /// Protection payments (CDS).
    Protection = 5,
    /// Recoveries after default.
    Recovery = 6,
    /// Exchanges of notional (cross-currency swaps).
    Exchange = 7,
}

/// Number of roles.
pub const ROLES: usize = 8;

/// One unit's flows over an interval, by role, in cash or in kind (PIK).
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct Flows {
    /// Cash flows per role.
    pub cash: [f64; ROLES],
    /// In-kind flows per role. They net to zero across roles (PIK: income +x, principal −x).
    pub in_kind: [f64; ROLES],
}

impl Flows {
    /// Adds a cash flow.
    pub fn add_cash(&mut self, role: Role, amount: f64) {
        self.cash[role as usize] += amount;
    }

    /// Adds an in-kind flow.
    pub fn add_in_kind(&mut self, role: Role, amount: f64) {
        self.in_kind[role as usize] += amount;
    }

    /// Total cash.
    #[must_use]
    pub fn cash_total(&self) -> f64 {
        self.cash.iter().sum()
    }

    /// Cash plus in kind for one role.
    #[must_use]
    pub fn of(&self, role: Role) -> f64 {
        self.cash[role as usize] + self.in_kind[role as usize]
    }
}

/// One unit's P&L over an interval, split by leg role.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct Terms {
    /// Interest earned.
    pub income: f64,
    /// Price change on the balance that remains, plus performance flows.
    pub price: f64,
    /// Settling balance at its paydown price instead of the mark.
    pub principal: f64,
    /// Fees, protection, recovery, exchange.
    pub other: f64,
    /// Financing, as a cost.
    pub financing: f64,
}

impl Terms {
    /// The terms from the unit's accrued interest, clean value and flows:
    ///
    /// * income = ΔA_inc + F_income
    /// * price = Δclean + R + F_performance
    /// * principal = F_principal − R
    /// * other = F_fee + F_protection + F_recovery + F_exchange
    /// * financing = ΔA_fund − F_funding
    ///
    /// `retired` is R, the mark value of the balance the interval retired (negative when it
    /// grew). With V = clean + A_inc − A_fund the terms add up to ΔV + ΣF exactly.
    #[must_use]
    pub fn from_parts(accrued_income: (f64, f64), accrued_funding: (f64, f64), clean: (f64, f64), retired: f64, f: &Flows) -> Self {
        Self {
            income: (accrued_income.1 - accrued_income.0) + f.of(Role::Income),
            price: (clean.1 - clean.0) + retired + f.of(Role::Performance),
            principal: f.of(Role::Principal) - retired,
            other: f.of(Role::Fee) + f.of(Role::Protection) + f.of(Role::Recovery) + f.of(Role::Exchange),
            financing: (accrued_funding.1 - accrued_funding.0) - f.of(Role::Funding),
        }
    }

    /// Total P&L: income + price + principal + other − financing.
    #[must_use]
    pub fn total(&self) -> f64 {
        self.income + self.price + self.principal + self.other - self.financing
    }
}

/// Whether a unit's value could be established.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ValuationState {
    /// Observed.
    #[default]
    Valid = 0,
    /// Carried forward under a missing-data policy.
    Stale = 1,
    /// Missing, with no policy to cover it.
    Missing = 2,
    /// The obligor has defaulted.
    Defaulted = 3,
    /// The contract has matured.
    Matured = 4,
    /// Not yet issued: the unit has no value, so nothing can trade or hold it.
    Pending = 5,
}

/// Cash a unit became entitled to in an interval but that pays later: a coupon gone ex before its
/// payment date, an agency pool's delayed remittance, a recovery settled after its determination.
/// The holder at the entitlement owns it, whoever holds the unit when it pays.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Deferred {
    /// When it pays.
    pub pay: Ts,
    /// Cash per unit.
    pub cash: f64,
    /// The part of it that is income (interest, coupons).
    pub income: f64,
}

/// One unit over one interval.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct UnitStep {
    /// Valuation state at the end of the interval.
    pub state: ValuationState,
    /// When everything this step used was known.
    pub known: Ts,
    /// The unit's value at the end of the interval.
    pub value: f64,
    /// Flows in the interval.
    pub flows: Flows,
    /// P&L terms over the interval.
    pub terms: Terms,
    /// Principal outstanding per unit at the end of the interval (current face per unit of
    /// original face), where the contract has one; NaN otherwise.
    pub face: f64,
    /// Cash entitled in the interval that pays later. Its P&L is in `terms` with the rest; its
    /// cash is not in `flows`.
    pub deferred: Option<Deferred>,
}

impl Default for UnitStep {
    fn default() -> Self {
        Self { state: ValuationState::default(), known: Ts::MIN, value: 0.0, flows: Flows::default(), terms: Terms::default(), face: f64::NAN, deferred: None }
    }
}

/// What a step needs besides the contract's own series.
pub struct StepCtx<'a> {
    /// What to do about a missing value.
    pub missing: MissingPolicy,
    /// Where to count what was not observed (I8).
    pub diagnostics: &'a Diagnostics,
    pub(crate) broken: &'a std::sync::Mutex<Option<String>>,
}

impl StepCtx<'_> {
    /// Reports an input the contract cannot step: a repayment larger than the balance, a flow
    /// that contradicts its terms. Computing the records then fails with the first report.
    pub fn fail(&self, message: impl Into<String>) {
        if let Ok(mut b) = self.broken.lock()
            && b.is_none()
        {
            *b = Some(message.into());
        }
    }

    /// Applies the missing-data policy to a read: the observed value, or the policy's answer.
    ///
    /// `last` is the last good value and `last_ok` when it was observed.
    #[must_use]
    pub fn resolve(&self, hit: Option<Hit>, last: f64, last_ok: Ts, now: Ts) -> (f64, ValuationState) {
        if let Some(h) = hit {
            return (h.value, ValuationState::Valid);
        }
        match self.missing {
            MissingPolicy::Fail => {
                Diagnostics::count(&self.diagnostics.missing);
                (f64::NAN, ValuationState::Missing)
            }
            MissingPolicy::Carry { max_age } => {
                if last.is_finite() && max_age.is_none_or(|a| now.0 - last_ok.0 <= a) {
                    Diagnostics::count(&self.diagnostics.stale);
                    (last, ValuationState::Stale)
                } else {
                    Diagnostics::count(&self.diagnostics.missing);
                    (f64::NAN, ValuationState::Missing)
                }
            }
            MissingPolicy::Default { value } => {
                Diagnostics::count(&self.diagnostics.defaults);
                (value, ValuationState::Stale)
            }
        }
    }
}

/// An instrument: a per-unit state machine over observations.
pub trait Contract: Send + Sync {
    /// The entity this contract is.
    fn entity(&self) -> EntityId;
    /// A short name for the definition (for reports).
    fn kind(&self) -> &'static str;
    /// Hash of the definition's parameters.
    fn params(&self) -> u64;
    /// Resolves the series the contract reads, once per pass over the calendar.
    ///
    /// # Errors
    /// If a series or column it needs does not exist.
    fn bind<'a>(&'a self, ws: &'a Workspace) -> Result<Box<dyn Stepper + 'a>>;
    /// The currency the unit is valued and pays in; `None` is the book's base currency.
    fn currency(&self) -> Option<EntityId> {
        None
    }
    /// Whether a short position needs a borrowed security (true for securities, false for
    /// derivatives and cash).
    fn needs_borrow(&self) -> bool {
        true
    }
    /// The series the contract reads (for the coherence of observed and derived state).
    fn reads(&self) -> Vec<String> {
        Vec::new()
    }
    /// The series its value comes from (its marks; default: everything it reads). A value takes
    /// observed marks, a pricing model's, a book's, a stress's or a simulated market's, never a
    /// signal's (spec, change 6).
    fn marks(&self) -> Vec<String> {
        self.reads()
    }
    /// What the definition models: its events and analytics, for the support matrix and the
    /// check of a run's requirements (spec, change 7). A requirement it does not list is refused,
    /// unless the run declares it an approximation.
    fn supports(&self) -> Vec<&'static str> {
        Vec::new()
    }
}

/// Any contract, valued and paying in a given currency.
pub struct InCurrency {
    /// The contract.
    pub contract: std::sync::Arc<dyn Contract>,
    /// Its currency (an entity of kind `currency`).
    pub currency: EntityId,
}

impl Contract for InCurrency {
    fn entity(&self) -> EntityId {
        self.contract.entity()
    }
    fn kind(&self) -> &'static str {
        self.contract.kind()
    }
    fn params(&self) -> u64 {
        let mut h = crate::hash::Hash64::new();
        h.u64(self.contract.params());
        h.u64(u64::from(self.currency.0));
        h.finish()
    }
    fn bind<'a>(&'a self, ws: &'a Workspace) -> Result<Box<dyn Stepper + 'a>> {
        self.contract.bind(ws)
    }
    fn currency(&self) -> Option<EntityId> {
        Some(self.currency)
    }
    fn needs_borrow(&self) -> bool {
        self.contract.needs_borrow()
    }
    fn reads(&self) -> Vec<String> {
        self.contract.reads()
    }
    fn marks(&self) -> Vec<String> {
        self.contract.marks()
    }
    fn supports(&self) -> Vec<&'static str> {
        let mut s = self.contract.supports();
        s.push("currency");
        s
    }
}

/// A contract bound to its series, stepping one unit through the calendar.
pub trait Stepper: Send {
    /// The unit at the first calendar time (flows and terms zero).
    fn init(&mut self, t0: Ts, ctx: &StepCtx<'_>) -> UnitStep;
    /// The unit over (`from`, `to`].
    fn step(&mut self, from: Ts, to: Ts, ctx: &StepCtx<'_>) -> UnitStep;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terms_add_up_for_a_pik_and_a_paydown() {
        // Clean 97 → 98 on balance 1.0 → 0.98 + 0.01 PIK; accrued 0.5 → 0.2; coupon 0.4 cash.
        let mut f = Flows::default();
        f.add_cash(Role::Income, 0.4);
        f.add_cash(Role::Principal, 0.02); // paydown of 0.02 face at par
        f.add_in_kind(Role::Income, 0.01); // PIK interest
        f.add_in_kind(Role::Principal, -0.01); // capitalised at par
        let mark0 = 0.97;
        let retired = mark0 * (0.02 - 0.01);
        let clean = (0.97 * 1.0, 0.98 * 0.99);
        let t = Terms::from_parts((0.5, 0.2), (0.0, 0.0), clean, retired, &f);
        let dv = (clean.1 + 0.2) - (clean.0 + 0.5);
        assert!((t.total() - (dv + f.cash_total())).abs() < 1e-15);
        assert!((t.principal - (0.02 - 0.01 - retired)).abs() < 1e-15);
    }
}
