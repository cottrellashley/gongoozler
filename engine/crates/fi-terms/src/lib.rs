//! Fixed income terms, as definitions a user composes (review C1–C3, `spec/engine/debt.md`).
//!
//! * [`date`]: calendar dates, month arithmetic and the end-of-month rule;
//! * [`daycount`]: year fractions by convention (ACT/360, ACT/365F, ACT/ACT ISDA and ICMA, the
//!   30/360 family);
//! * [`calendar`]: business days from declared weekends and holidays, and business-day adjustment;
//! * [`schedule`]: accrual and payment periods, with stubs, the end-of-month rule, payment lags and
//!   ex-coupon dates;
//! * [`coupon`]: how a period's rate is set: fixed, stepped, floating on fixings (in advance, or
//!   compounded or averaged in arrears with lookback and lockout), inflation-indexed, zero;
//! * [`debt`]: a debt instrument's terms: its coupon leg, its principal (bullet, amortising,
//!   sinking), its rights (calls, puts, extension, conversion), its settlement and quotation;
//! * [`curve`]: discount curves from observed points, with declared interpolation;
//! * [`analytics`]: prices from curves and yields, yield and spread solvers, and sensitivities by
//!   declared bumps.
//!
//! Nothing here names a country, a market or a data source: calendars, curves and fixings are what
//! the user declares. The crate depends on nothing, so the same terms price a bond in a run, in a
//! notebook, or in another engine.

pub mod analytics;
pub mod calendar;
pub mod coupon;
pub mod curve;
pub mod date;
pub mod daycount;
pub mod debt;
pub mod schedule;

pub use analytics::{Bump, BumpKind, Sensitivity, YieldConvention};
pub use calendar::{BusinessCalendar, BusinessDay};
pub use coupon::{Coupon, Fixings, Method};
pub use curve::{Curve, Interpolation};
pub use date::Date;
pub use daycount::DayCount;
pub use debt::{Amortisation, Cashflow, CashflowKind, Debt, Extension, Price, Right, RightKind};
pub use schedule::{Period, Schedule, Stub};

use std::fmt;

/// What can go wrong with terms: they contradict themselves, or a value they need is missing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The terms are inconsistent: a maturity before issue, an unknown convention.
    Invalid(String),
    /// A fixing, a curve point or another input the terms need is not available.
    Missing(String),
    /// A solver did not converge.
    NoSolution(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(m) => write!(f, "invalid terms: {m}"),
            Self::Missing(m) => write!(f, "missing: {m}"),
            Self::NoSolution(m) => write!(f, "no solution: {m}"),
        }
    }
}

impl std::error::Error for Error {}

/// A result with this crate's error.
pub type Result<T> = std::result::Result<T, Error>;
