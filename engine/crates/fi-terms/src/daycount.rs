//! Day counts: how a period's interest is measured as a fraction of a year.

use std::fmt;
use std::str::FromStr;

use crate::date::{Date, days_in_month};
use crate::{Error, Result};

/// A day-count convention (ISDA 2006 definitions, section 4.16, and ICMA Rule 251).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum DayCount {
    /// Actual days over 360 (money markets, floating legs).
    Act360,
    /// Actual days over 365, whatever the year (sterling money markets, many curves).
    Act365F,
    /// Actual days over 365.25.
    Act36525,
    /// Actual/actual (ISDA): days in each calendar year over that year's length.
    ActActIsda,
    /// Actual/actual (ICMA): days over the length of the coupon period, times the coupons a year.
    /// Needs the period ([`Reference`]); most government and corporate bonds.
    ActActIcma,
    /// 30/360, bond basis (ISDA 4.16(f)): a 31st becomes the 30th; the end date's 31st only when
    /// the start was the 30th or 31st.
    Thirty360,
    /// 30/360 US (SIA): bond basis, plus the February rules for bonds paying on the last day of
    /// February.
    Thirty360Us,
    /// 30E/360, Eurobond basis (ISDA 4.16(g)): every 31st becomes the 30th.
    Thirty360E,
    /// 30E/360 ISDA (4.16(h)): every month end becomes the 30th, except a February end that is the
    /// termination date.
    Thirty360EIsda,
}

/// The coupon period an ICMA fraction is measured against, and the coupons a year.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Reference {
    /// The period's start (regular, or notional for a stub).
    pub start: Date,
    /// The period's end.
    pub end: Date,
    /// Coupons a year.
    pub per_year: u32,
}

impl DayCount {
    /// The year fraction from `d1` to `d2`. ACT/ACT ICMA needs the coupon period it is measured
    /// against; 30E/360 ISDA needs the termination date, when the period ends on it.
    ///
    /// # Errors
    /// ACT/ACT ICMA without a reference period.
    pub fn fraction(self, d1: Date, d2: Date, reference: Option<Reference>, termination: Option<Date>) -> Result<f64> {
        if d2 < d1 {
            return self.fraction(d2, d1, reference, termination).map(|f| -f);
        }
        let days = f64::from(d1.days_to(d2));
        Ok(match self {
            Self::Act360 => days / 360.0,
            Self::Act365F => days / 365.0,
            Self::Act36525 => days / 365.25,
            Self::ActActIsda => act_act_isda(d1, d2),
            Self::ActActIcma => {
                let r = reference.ok_or_else(|| Error::Invalid("ACT/ACT ICMA needs the coupon period it is measured against".into()))?;
                icma(d1, d2, r)?
            }
            Self::Thirty360 | Self::Thirty360Us | Self::Thirty360E | Self::Thirty360EIsda => thirty(self, d1, d2, termination) / 360.0,
        })
    }

    /// Whether the convention needs the coupon period (ACT/ACT ICMA).
    #[must_use]
    pub fn needs_reference(self) -> bool {
        self == Self::ActActIcma
    }

    /// The convention's usual name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Act360 => "ACT/360",
            Self::Act365F => "ACT/365F",
            Self::Act36525 => "ACT/365.25",
            Self::ActActIsda => "ACT/ACT ISDA",
            Self::ActActIcma => "ACT/ACT ICMA",
            Self::Thirty360 => "30/360",
            Self::Thirty360Us => "30/360 US",
            Self::Thirty360E => "30E/360",
            Self::Thirty360EIsda => "30E/360 ISDA",
        }
    }
}

impl fmt::Display for DayCount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for DayCount {
    type Err = Error;

    /// Reads the usual names, in any case: `ACT/360`, `ACT/365F` (or `ACT/365`), `ACT/365.25`,
    /// `ACT/ACT ISDA` (or `ACT/ACT`), `ACT/ACT ICMA` (or `ISMA`), `30/360` (bond basis),
    /// `30/360 US`, `30E/360`, `30E/360 ISDA`.
    fn from_str(s: &str) -> Result<Self> {
        let k: String = s.trim().to_ascii_uppercase().replace("ACTUAL", "ACT").replace(['_', '-'], " ").split_whitespace().collect::<Vec<_>>().join(" ");
        Ok(match k.as_str() {
            "ACT/360" | "A/360" => Self::Act360,
            "ACT/365F" | "ACT/365 F" | "ACT/365" | "A/365F" | "ACT/365 FIXED" => Self::Act365F,
            "ACT/365.25" => Self::Act36525,
            "ACT/ACT" | "ACT/ACT ISDA" | "A/A ISDA" => Self::ActActIsda,
            "ACT/ACT ICMA" | "ACT/ACT ISMA" | "A/A ICMA" => Self::ActActIcma,
            "30/360" | "30/360 BOND" | "30/360 BOND BASIS" | "30/360 ISDA" => Self::Thirty360,
            "30/360 US" | "30/360 SIA" | "30U/360" => Self::Thirty360Us,
            "30E/360" | "30/360 E" | "EUROBOND" => Self::Thirty360E,
            "30E/360 ISDA" => Self::Thirty360EIsda,
            _ => {
                return Err(Error::Invalid(format!(
                    "no day count {s:?} (ACT/360, ACT/365F, ACT/365.25, ACT/ACT ISDA, ACT/ACT ICMA, 30/360, 30/360 US, 30E/360, 30E/360 ISDA)"
                )));
            }
        })
    }
}

fn act_act_isda(d1: Date, d2: Date) -> f64 {
    let (y1, y2) = (d1.year(), d2.year());
    let len = |y: i32| if Date::is_leap(y) { 366.0 } else { 365.0 };
    if y1 == y2 {
        return f64::from(d1.days_to(d2)) / len(y1);
    }
    let end1 = Date::from_ymd(y1 + 1, 1, 1).expect("a year start");
    let start2 = Date::from_ymd(y2, 1, 1).expect("a year start");
    f64::from(d1.days_to(end1)) / len(y1) + f64::from(y2 - y1 - 1) + f64::from(start2.days_to(d2)) / len(y2)
}

/// ACT/ACT ICMA (Rule 251): days over (coupons a year × days in the period). A stub longer or
/// shorter than its period is measured against notional periods running from the regular dates.
fn icma(d1: Date, d2: Date, r: Reference) -> Result<f64> {
    if r.per_year == 0 || r.end <= r.start || !12u32.is_multiple_of(r.per_year) {
        return Err(Error::Invalid(format!("an ICMA reference period needs coupons a year dividing 12, not {}", r.per_year)));
    }
    let f = f64::from(r.per_year);
    if d1 >= r.start && d2 <= r.end {
        return Ok(f64::from(d1.days_to(d2)) / (f * f64::from(r.start.days_to(r.end))));
    }
    let months = (12 / r.per_year) as i32;
    let mut total = 0.0;
    // Notional periods backward from the reference's end, for what lies before it...
    let (mut end, mut k) = (r.end, 0);
    while end > d1 && k < 1_000 {
        let start = r.end.plus_months(-(k + 1) * months, false);
        let (a, b) = (d1.max(start), d2.min(end));
        if b > a {
            total += f64::from(a.days_to(b)) / (f * f64::from(start.days_to(end)));
        }
        end = start;
        k += 1;
    }
    // ...and forward from it, for what lies after.
    let (mut start, mut k) = (r.end, 0);
    while start < d2 && k < 1_000 {
        let end = r.end.plus_months((k + 1) * months, false);
        let (a, b) = (d1.max(start), d2.min(end));
        if b > a {
            total += f64::from(a.days_to(b)) / (f * f64::from(start.days_to(end)));
        }
        start = end;
        k += 1;
    }
    Ok(total)
}

fn last_of_february(d: Date) -> bool {
    d.month() == 2 && d.day() == days_in_month(d.year(), 2)
}

fn thirty(dc: DayCount, d1: Date, d2: Date, termination: Option<Date>) -> f64 {
    let (y1, m1, mut a) = d1.ymd();
    let (y2, m2, mut b) = d2.ymd();
    match dc {
        DayCount::Thirty360 => {
            if a == 31 {
                a = 30;
            }
            if b == 31 && a == 30 {
                b = 30;
            }
        }
        DayCount::Thirty360Us => {
            if last_of_february(d1) {
                if last_of_february(d2) {
                    b = 30;
                }
                a = 30;
            }
            if b == 31 && a >= 30 {
                b = 30;
            }
            if a == 31 {
                a = 30;
            }
        }
        DayCount::Thirty360E => {
            a = a.min(30);
            b = b.min(30);
        }
        DayCount::Thirty360EIsda => {
            if d1.is_month_end() {
                a = 30;
            }
            if d2.is_month_end() && !(Some(d2) == termination && m2 == 2) {
                b = 30;
            }
        }
        _ => unreachable!("not a 30/360 convention"),
    }
    f64::from(360 * (y2 - y1)) + f64::from(30 * (m2 as i32 - m1 as i32)) + f64::from(b as i32 - a as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Date {
        s.parse().unwrap()
    }

    fn close(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-12, "{a} ≠ {b}");
    }

    #[test]
    fn the_classic_period_under_each_actual_convention() {
        // 1 Nov 2003 to 1 May 2004: 61 days in 2003, 121 in the leap year 2004 (ISDA's example).
        let (a, b) = (d("2003-11-01"), d("2004-05-01"));
        close(DayCount::ActActIsda.fraction(a, b, None, None).unwrap(), 61.0 / 365.0 + 121.0 / 366.0);
        let r = Reference { start: a, end: b, per_year: 2 };
        close(DayCount::ActActIcma.fraction(a, b, Some(r), None).unwrap(), 0.5);
        close(DayCount::Act365F.fraction(a, b, None, None).unwrap(), 182.0 / 365.0);
        close(DayCount::Act360.fraction(a, b, None, None).unwrap(), 182.0 / 360.0);
        assert!(DayCount::ActActIcma.fraction(a, b, None, None).is_err(), "ICMA needs its period");
    }

    #[test]
    fn icma_measures_stubs_against_notional_periods() {
        // A short first period, 1 Feb 1999 to 1 Jul 1999, annual coupons: 150 days of a 365-day year.
        let r = Reference { start: d("1998-07-01"), end: d("1999-07-01"), per_year: 1 };
        close(DayCount::ActActIcma.fraction(d("1999-02-01"), d("1999-07-01"), Some(r), None).unwrap(), 150.0 / 365.0);
        // A long first period, 15 Aug 2002 to 15 Jul 2003, semiannual coupons on 15 Jan and 15 Jul:
        // 153 of the 184 days of the notional period to 15 Jan 2003, then the 181 of the regular
        // half-year: (153/184 + 181/181) / 2.
        let r = Reference { start: d("2003-01-15"), end: d("2003-07-15"), per_year: 2 };
        close(DayCount::ActActIcma.fraction(d("2002-08-15"), d("2003-07-15"), Some(r), None).unwrap(), (153.0 / 184.0 + 1.0) / 2.0);
    }

    #[test]
    fn the_thirty_360_family_treats_month_ends_by_its_own_rules() {
        let f = |dc: DayCount, a: &str, b: &str, t: Option<&str>| dc.fraction(d(a), d(b), None, t.map(d)).unwrap() * 360.0;
        // 31 Jan to 28 Feb 2007.
        close(f(DayCount::Thirty360, "2007-01-31", "2007-02-28", None), 28.0);
        close(f(DayCount::Thirty360E, "2007-01-31", "2007-02-28", None), 28.0);
        close(f(DayCount::Thirty360EIsda, "2007-01-31", "2007-02-28", None), 30.0);
        close(f(DayCount::Thirty360EIsda, "2007-01-31", "2007-02-28", Some("2007-02-28")), 28.0);
        // 28 Feb to 31 Aug 2007.
        close(f(DayCount::Thirty360, "2007-02-28", "2007-08-31", None), 183.0);
        close(f(DayCount::Thirty360E, "2007-02-28", "2007-08-31", None), 182.0);
        close(f(DayCount::Thirty360Us, "2007-02-28", "2007-08-31", None), 180.0);
        close(f(DayCount::Thirty360EIsda, "2007-02-28", "2007-08-31", None), 180.0);
        // 30 Mar to 31 Mar: the end's 31st becomes the 30th only after a 30th or 31st.
        close(f(DayCount::Thirty360, "2007-03-30", "2007-03-31", None), 0.0);
        close(f(DayCount::Thirty360, "2007-03-29", "2007-03-31", None), 2.0);
    }

    #[test]
    fn names_read_back_and_backwards_periods_are_negative() {
        for dc in [
            DayCount::Act360,
            DayCount::Act365F,
            DayCount::Act36525,
            DayCount::ActActIsda,
            DayCount::ActActIcma,
            DayCount::Thirty360,
            DayCount::Thirty360Us,
            DayCount::Thirty360E,
            DayCount::Thirty360EIsda,
        ] {
            assert_eq!(dc.name().parse::<DayCount>().unwrap(), dc);
        }
        assert_eq!("actual/365 fixed".parse::<DayCount>().unwrap(), DayCount::Act365F);
        assert!("BUS/252".parse::<DayCount>().is_err());
        close(DayCount::Act360.fraction(d("2024-03-01"), d("2024-01-01"), None, None).unwrap(), -60.0 / 360.0);
    }
}
