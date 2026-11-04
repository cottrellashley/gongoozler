//! How a period's coupon rate is set.

use crate::calendar::BusinessCalendar;
use crate::date::{Date, days_in_month};
use crate::schedule::Period;
use crate::{Error, Result};

/// Observed fixings and index levels, as of whatever the caller's view allows. A rate is a
/// decimal per year (0.0525); an inflation index is a level for the month whose first day is
/// given.
pub trait Fixings {
    /// The value of `index` for `date`, if it is known.
    fn fixing(&self, index: &str, date: Date) -> Option<f64>;
}

/// No fixings at all: enough for fixed and zero coupons.
pub struct NoFixings;

impl Fixings for NoFixings {
    fn fixing(&self, _: &str, _: Date) -> Option<f64> {
        None
    }
}

/// How a floating rate is observed for a period.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Method {
    /// One fixing, `lag` business days before the period starts (a term rate: Euribor, 2).
    InAdvance { lag: i32 },
    /// Daily fixings compounded over the period, each observed `lookback` business days earlier;
    /// the last `lockout` business days take the rate fixed when the lockout starts (SOFR, SONIA).
    CompoundedInArrears { lookback: i32, lockout: i32 },
    /// Daily fixings averaged over the period, weighted by the days each applies, with the same
    /// lookback and lockout.
    AverageInArrears { lookback: i32, lockout: i32 },
}

/// How a period's coupon is set.
#[derive(Clone, Debug, PartialEq)]
pub enum Coupon {
    /// A fixed rate per year (0.05 for 5%).
    Fixed { rate: f64 },
    /// A rate for the periods starting on or after each date, in order.
    Step { rates: Vec<(Date, f64)> },
    /// `gearing × index + spread`, capped and floored, observed by `method` on the fixing
    /// calendar.
    Floating { index: String, spread: f64, gearing: f64, cap: Option<f64>, floor: Option<f64>, method: Method, calendar: BusinessCalendar },
    /// A real rate on principal indexed to `index` (monthly levels), from a `base` level, with a
    /// `lag` in months and, with `interpolate`, daily interpolation between months. With `floor`,
    /// principal is repaid at no less than par.
    Inflation { index: String, real: f64, base: f64, lag: u32, interpolate: bool, floor: bool },
    /// No coupon.
    Zero,
}

impl Coupon {
    /// The rate per year for `period`.
    ///
    /// # Errors
    /// A fixing the period needs that `fixings` does not have, or a step schedule with no rate
    /// for the period.
    pub fn rate(&self, period: &Period, day_count_days: impl Fn(Date, Date) -> f64, fixings: &dyn Fixings) -> Result<f64> {
        match self {
            Self::Fixed { rate } => Ok(*rate),
            Self::Zero => Ok(0.0),
            Self::Step { rates } => rates
                .iter()
                .rev()
                .find(|(from, _)| *from <= period.start)
                .map(|(_, r)| *r)
                .ok_or_else(|| Error::Invalid(format!("no stepped rate for the period starting {}", period.start))),
            Self::Inflation { real, .. } => Ok(*real),
            Self::Floating { index, spread, gearing, cap, floor, method, calendar } => {
                let base = observed(index, *method, calendar, period, &day_count_days, fixings)?;
                let mut r = gearing * base + spread;
                if let Some(f) = floor {
                    r = r.max(*f);
                }
                if let Some(c) = cap {
                    r = r.min(*c);
                }
                Ok(r)
            }
        }
    }

    /// The index ratio an inflation-indexed amount on `date` is scaled by (1 otherwise).
    ///
    /// # Errors
    /// An index level that `fixings` does not have.
    pub fn index_ratio(&self, date: Date, fixings: &dyn Fixings) -> Result<f64> {
        match self {
            Self::Inflation { index, base, lag, interpolate, .. } => Ok(reference_index(index, date, *lag, *interpolate, fixings)? / base),
            _ => Ok(1.0),
        }
    }

    /// Whether principal is repaid at no less than par, whatever the index did.
    #[must_use]
    pub fn floors_principal(&self) -> bool {
        matches!(self, Self::Inflation { floor: true, .. })
    }

    /// The fixings index this coupon reads, if any.
    #[must_use]
    pub fn index(&self) -> Option<&str> {
        match self {
            Self::Floating { index, .. } | Self::Inflation { index, .. } => Some(index),
            _ => None,
        }
    }
}

/// The reference index for `date`: the level `lag` months before, interpolated through the
/// month by day when asked (the convention of most inflation-linked government bonds).
fn reference_index(index: &str, date: Date, lag: u32, interpolate: bool, fixings: &dyn Fixings) -> Result<f64> {
    let first = Date::from_ymd(date.year(), date.month(), 1).expect("a month start");
    let month = first.plus_months(-(lag as i32), false);
    let level = |m: Date| fixings.fixing(index, m).ok_or_else(|| Error::Missing(format!("{index} for {}-{:02}", m.year(), m.month())));
    let i0 = level(month)?;
    if !interpolate || date.day() == 1 {
        return Ok(i0);
    }
    let i1 = level(month.plus_months(1, false))?;
    let n = f64::from(days_in_month(date.year(), date.month()));
    Ok(i0 + (f64::from(date.day()) - 1.0) / n * (i1 - i0))
}

fn observed(index: &str, method: Method, cal: &BusinessCalendar, p: &Period, dcf: &dyn Fn(Date, Date) -> f64, fixings: &dyn Fixings) -> Result<f64> {
    let fix = |d: Date| fixings.fixing(index, d).ok_or_else(|| Error::Missing(format!("{index} fixing for {d}")));
    match method {
        Method::InAdvance { lag } => fix(cal.add_business_days(cal.adjust(p.start, crate::BusinessDay::Following), -lag)),
        Method::CompoundedInArrears { lookback, lockout } | Method::AverageInArrears { lookback, lockout } => {
            // The business days of the period, each applying until the next.
            let mut days = Vec::new();
            let mut d = cal.adjust(p.start, crate::BusinessDay::Following);
            while d < p.end {
                days.push(d);
                d = cal.add_business_days(d, 1);
            }
            if days.is_empty() {
                return Err(Error::Invalid(format!("no business day in the period {} to {}", p.start, p.end)));
            }
            let lock_from = days.len().saturating_sub(lockout.max(0) as usize).max(1).min(days.len());
            let mut growth = 1.0;
            let mut sum = 0.0;
            let mut fixed_at_lock = None;
            for (i, &day) in days.iter().enumerate() {
                let next = days.get(i + 1).copied().unwrap_or(p.end);
                let r = if lockout > 0 && i >= lock_from {
                    match fixed_at_lock {
                        Some(r) => r,
                        None => {
                            let r = fix(cal.add_business_days(days[lock_from - 1], -lookback))?;
                            fixed_at_lock = Some(r);
                            r
                        }
                    }
                } else {
                    fix(cal.add_business_days(day, -lookback))?
                };
                let tau = dcf(day, next);
                growth *= 1.0 + r * tau;
                sum += r * tau;
            }
            let total = dcf(p.start, p.end);
            Ok(match method {
                Method::CompoundedInArrears { .. } => (growth - 1.0) / total,
                _ => sum / total,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::daycount::{DayCount, Reference};

    struct Map(HashMap<(String, Date), f64>);

    impl Fixings for Map {
        fn fixing(&self, index: &str, date: Date) -> Option<f64> {
            self.0.get(&(index.to_string(), date)).copied()
        }
    }

    fn d(s: &str) -> Date {
        s.parse().unwrap()
    }

    fn period(a: &str, b: &str) -> Period {
        Period { start: d(a), end: d(b), pay: d(b), ex: None, reference: Reference { start: d(a), end: d(b), per_year: 4 }, regular: true }
    }

    fn act360(a: Date, b: Date) -> f64 {
        DayCount::Act360.fraction(a, b, None, None).unwrap()
    }

    #[test]
    fn a_term_rate_fixes_in_advance_with_its_lag_spread_floor_and_cap() {
        let mut m = HashMap::new();
        m.insert(("EUR3M".into(), d("2026-01-13")), -0.004);
        let fx = Map(m);
        let c = Coupon::Floating {
            index: "EUR3M".into(),
            spread: 0.0,
            gearing: 1.0,
            cap: None,
            floor: Some(0.0),
            method: Method::InAdvance { lag: 2 },
            calendar: BusinessCalendar::weekends(),
        };
        // The period starts Thursday 15 Jan 2026; two business days before is Tuesday 13 Jan.
        assert_eq!(c.rate(&period("2026-01-15", "2026-04-15"), act360, &fx).unwrap(), 0.0, "a negative fixing with a contractual floor");
        let c = Coupon::Floating {
            index: "EUR3M".into(),
            spread: 0.01,
            gearing: 1.0,
            cap: Some(0.0055),
            floor: None,
            method: Method::InAdvance { lag: 2 },
            calendar: BusinessCalendar::weekends(),
        };
        assert!((c.rate(&period("2026-01-15", "2026-04-15"), act360, &fx).unwrap() - 0.0055).abs() < 1e-15, "capped");
        assert!(matches!(c.rate(&period("2026-04-15", "2026-07-15"), act360, &fx), Err(Error::Missing(_))), "an unknown fixing is not zero");
    }

    #[test]
    fn overnight_rates_compound_in_arrears_with_lookback_and_lockout() {
        // A week: Monday 2 to Monday 9 Feb 2026, a flat 4% except Friday's 5%.
        let mut m = HashMap::new();
        let days = ["2026-01-26", "2026-01-27", "2026-01-28", "2026-01-29", "2026-01-30", "2026-02-02", "2026-02-03", "2026-02-04", "2026-02-05", "2026-02-06"];
        for day in days {
            m.insert(("SOFR".into(), d(day)), if day == "2026-02-06" { 0.05 } else { 0.04 });
        }
        let fx = Map(m);
        let p = period("2026-02-02", "2026-02-09");
        let comp = |lookback, lockout| {
            Coupon::Floating {
                index: "SOFR".into(),
                spread: 0.0,
                gearing: 1.0,
                cap: None,
                floor: None,
                method: Method::CompoundedInArrears { lookback, lockout },
                calendar: BusinessCalendar::weekends(),
            }
            .rate(&p, act360, &fx)
            .unwrap()
        };
        // No lookback: four days at 4% for one day each, Friday at 5% for three days.
        let want = ((1.0f64 + 0.04 / 360.0).powi(4) * (1.0 + 0.05 * 3.0 / 360.0) - 1.0) * 360.0 / 7.0;
        assert!((comp(0, 0) - want).abs() < 1e-14);
        // A two-day lockout: Thursday and Friday take Wednesday's rate (4%).
        let locked = ((1.0f64 + 0.04 / 360.0).powi(4) * (1.0 + 0.04 * 3.0 / 360.0) - 1.0) * 360.0 / 7.0;
        assert!((comp(0, 2) - locked).abs() < 1e-14);
        // A five-day lookback reads the week before, all at 4%.
        let back = ((1.0f64 + 0.04 / 360.0).powi(4) * (1.0 + 0.04 * 3.0 / 360.0) - 1.0) * 360.0 / 7.0;
        assert!((comp(5, 0) - back).abs() < 1e-14);
    }

    #[test]
    fn inflation_indexes_principal_with_a_lag_and_daily_interpolation() {
        let mut m = HashMap::new();
        m.insert(("CPI".into(), d("2025-10-01")), 300.0);
        m.insert(("CPI".into(), d("2025-11-01")), 303.0);
        let fx = Map(m);
        let c = Coupon::Inflation { index: "CPI".into(), real: 0.01, base: 250.0, lag: 3, interpolate: true, floor: true };
        // 16 Jan 2026, a three-month lag: October's level plus 15/31 of the way to November's.
        let want = (300.0 + 15.0 / 31.0 * 3.0) / 250.0;
        assert!((c.index_ratio(d("2026-01-16"), &fx).unwrap() - want).abs() < 1e-14);
        assert!(c.floors_principal());
        assert!(c.index_ratio(d("2026-03-16"), &fx).is_err(), "January's level is not published");
    }
}
