//! Calendar dates: whole days since 1970-01-01, proleptic Gregorian.

use std::fmt;
use std::str::FromStr;

use crate::{Error, Result};

/// A calendar date, as days since 1970-01-01.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date(pub i32);

impl Date {
    /// The date of `year`-`month`-`day`.
    ///
    /// # Errors
    /// A month outside 1–12, or a day the month does not have.
    pub fn from_ymd(year: i32, month: u32, day: u32) -> Result<Self> {
        if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
            return Err(Error::Invalid(format!("no date {year:04}-{month:02}-{day:02}")));
        }
        Ok(Self(days_from_civil(year, month, day)))
    }

    /// (year, month, day).
    #[must_use]
    pub fn ymd(self) -> (i32, u32, u32) {
        civil_from_days(self.0)
    }

    /// The year.
    #[must_use]
    pub fn year(self) -> i32 {
        self.ymd().0
    }

    /// The month, 1–12.
    #[must_use]
    pub fn month(self) -> u32 {
        self.ymd().1
    }

    /// The day of the month.
    #[must_use]
    pub fn day(self) -> u32 {
        self.ymd().2
    }

    /// Day of the week: 0 is Monday, 6 is Sunday.
    #[must_use]
    pub fn weekday(self) -> u32 {
        // 1970-01-01 was a Thursday (3).
        (self.0 + 3).rem_euclid(7) as u32
    }

    /// `n` days later (earlier when negative).
    #[must_use]
    pub fn plus_days(self, n: i32) -> Self {
        Self(self.0 + n)
    }

    /// Days from `self` to `to`.
    #[must_use]
    pub fn days_to(self, to: Self) -> i32 {
        to.0 - self.0
    }

    /// Whether this is the last day of its month.
    #[must_use]
    pub fn is_month_end(self) -> bool {
        let (y, m, d) = self.ymd();
        d == days_in_month(y, m)
    }

    /// The last day of this date's month.
    #[must_use]
    pub fn month_end(self) -> Self {
        let (y, m, _) = self.ymd();
        Self(days_from_civil(y, m, days_in_month(y, m)))
    }

    /// `n` months later (earlier when negative). The day is kept when the month has it and cut
    /// to the month's last day otherwise; with `eom`, a month-end date stays a month-end date.
    #[must_use]
    pub fn plus_months(self, n: i32, eom: bool) -> Self {
        let (y, m, d) = self.ymd();
        let total = y * 12 + (m as i32 - 1) + n;
        let (ny, nm) = (total.div_euclid(12), total.rem_euclid(12) as u32 + 1);
        let last = days_in_month(ny, nm);
        let nd = if eom && d == days_in_month(y, m) { last } else { d.min(last) };
        Self(days_from_civil(ny, nm, nd))
    }

    /// Whether `year` is a leap year.
    #[must_use]
    pub fn is_leap(year: i32) -> bool {
        (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
    }
}

/// Days in `month` of `year`.
#[must_use]
pub fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if Date::is_leap(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// `YYYY-MM-DD`.
impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (y, m, d) = self.ymd();
        write!(f, "{y:04}-{m:02}-{d:02}")
    }
}

impl FromStr for Date {
    type Err = Error;

    /// Reads `YYYY-MM-DD`.
    fn from_str(s: &str) -> Result<Self> {
        let bad = || Error::Invalid(format!("{s:?} is not a date (YYYY-MM-DD)"));
        let mut it = s.trim().splitn(3, '-');
        let y = it.next().and_then(|p| p.parse::<i32>().ok()).ok_or_else(bad)?;
        let m = it.next().and_then(|p| p.parse::<u32>().ok()).ok_or_else(bad)?;
        let d = it.next().and_then(|p| p.parse::<u32>().ok()).ok_or_else(bad)?;
        Self::from_ymd(y, m, d)
    }
}

/// Days since 1970-01-01 to (year, month, day): Howard Hinnant's `civil_from_days`.
fn civil_from_days(z: i32) -> (i32, u32, u32) {
    let z = i64::from(z) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    ((y + i64::from(m <= 2)) as i32, m, d)
}

/// (year, month, day) to days since 1970-01-01: Howard Hinnant's `days_from_civil`.
fn days_from_civil(y: i32, m: u32, d: u32) -> i32 {
    let y = i64::from(y) - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146_097 + doe - 719_468) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Date {
        s.parse().unwrap()
    }

    #[test]
    fn dates_round_trip_and_know_their_weekday() {
        assert_eq!(d("1970-01-01").0, 0);
        assert_eq!(d("2024-02-29").to_string(), "2024-02-29");
        assert_eq!(d("2026-11-03").weekday(), 1, "a Tuesday");
        assert!(Date::from_ymd(2023, 2, 29).is_err());
        assert!("2023-13-01".parse::<Date>().is_err());
        for z in [-800_000, -1, 0, 1, 19_000, 2_000_000] {
            let x = Date(z);
            let (y, m, dd) = x.ymd();
            assert_eq!(Date::from_ymd(y, m, dd).unwrap(), x);
        }
    }

    #[test]
    fn months_keep_the_day_or_cut_it_and_the_end_of_month_rule_holds() {
        assert_eq!(d("2024-01-31").plus_months(1, false), d("2024-02-29"));
        assert_eq!(d("2024-02-29").plus_months(1, false), d("2024-03-29"));
        assert_eq!(d("2024-02-29").plus_months(1, true), d("2024-03-31"), "a month end stays a month end");
        assert_eq!(d("2024-03-31").plus_months(-6, false), d("2023-09-30"));
        assert_eq!(d("2023-05-15").plus_months(-17, false), d("2021-12-15"));
        assert!(d("2023-02-28").is_month_end() && !d("2024-02-28").is_month_end());
    }
}
