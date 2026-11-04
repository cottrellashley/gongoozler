//! Business days: declared weekends and holidays, and how a date that is not one is moved.
//!
//! No calendar here names a country. A calendar is what the user declares: a weekend, a list of
//! holidays, or rules that generate them (a fixed date with its weekend observance, the n-th
//! weekday of a month, a day relative to Easter).

use std::fmt;
use std::str::FromStr;

use crate::date::{Date, days_in_month};
use crate::{Error, Result};

/// How a date that is not a business day is moved (ISDA 2006, section 4.12).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum BusinessDay {
    /// Not moved.
    Unadjusted,
    /// To the next business day.
    #[default]
    Following,
    /// To the next business day, unless that is in the next month: then the previous one.
    ModifiedFollowing,
    /// To the previous business day.
    Preceding,
    /// To the previous business day, unless that is in the previous month: then the next one.
    ModifiedPreceding,
}

impl FromStr for BusinessDay {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        Ok(match s.trim().to_ascii_lowercase().replace(['_', '-', ' '], "").as_str() {
            "unadjusted" | "none" => Self::Unadjusted,
            "following" | "f" => Self::Following,
            "modifiedfollowing" | "mf" | "modfollowing" => Self::ModifiedFollowing,
            "preceding" | "p" => Self::Preceding,
            "modifiedpreceding" | "mp" => Self::ModifiedPreceding,
            _ => {
                return Err(Error::Invalid(format!(
                    "no business-day convention {s:?} (unadjusted, following, modified following, preceding, modified preceding)"
                )));
            }
        })
    }
}

impl fmt::Display for BusinessDay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unadjusted => "unadjusted",
            Self::Following => "following",
            Self::ModifiedFollowing => "modified following",
            Self::Preceding => "preceding",
            Self::ModifiedPreceding => "modified preceding",
        })
    }
}

/// When a holiday that falls on a weekend is observed.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Observance {
    /// On the day itself, even at a weekend (so no business day is lost).
    Actual,
    /// Saturday's on the Friday before, Sunday's on the Monday after.
    Nearest,
    /// On the next weekday.
    NextWeekday,
}

/// A rule generating one holiday a year.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Rule {
    /// A fixed month and day, observed as declared.
    Fixed { month: u32, day: u32, observance: Observance },
    /// The `n`-th `weekday` (0 is Monday) of `month`; `n = -1` is the last.
    NthWeekday { month: u32, weekday: u32, n: i32 },
    /// Easter Sunday (Western, Gregorian) plus `offset` days: −2 is Good Friday, 1 Easter Monday.
    Easter { offset: i32 },
}

impl Rule {
    /// The holiday the rule makes in `year`, if any.
    #[must_use]
    pub fn in_year(self, year: i32) -> Option<Date> {
        match self {
            Self::Fixed { month, day, observance } => {
                let d = Date::from_ymd(year, month, day).ok()?;
                Some(match (observance, d.weekday()) {
                    (Observance::Nearest, 5) => d.plus_days(-1),
                    (Observance::Nearest, 6) | (Observance::NextWeekday, 6) => d.plus_days(1),
                    (Observance::NextWeekday, 5) => d.plus_days(2),
                    _ => d,
                })
            }
            Self::NthWeekday { month, weekday, n } => {
                if n > 0 {
                    let first = Date::from_ymd(year, month, 1).ok()?;
                    let shift = (weekday + 7 - first.weekday()) % 7;
                    let d = first.plus_days((shift + 7 * (n as u32 - 1)) as i32);
                    (d.month() == month).then_some(d)
                } else {
                    let last = Date::from_ymd(year, month, days_in_month(year, month)).ok()?;
                    let back = (last.weekday() + 7 - weekday) % 7;
                    let d = last.plus_days(-(back as i32) - 7 * (-n - 1));
                    (d.month() == month).then_some(d)
                }
            }
            Self::Easter { offset } => Some(easter(year).plus_days(offset)),
        }
    }
}

/// Easter Sunday in `year` (the Gregorian computus).
#[must_use]
pub fn easter(year: i32) -> Date {
    let a = year % 19;
    let b = year / 100;
    let c = year % 100;
    let d = b / 4;
    let e = b % 4;
    let f = (b + 8) / 25;
    let g = (b - f + 1) / 3;
    let h = (19 * a + b - d - g + 15) % 30;
    let i = c / 4;
    let k = c % 4;
    let l = (32 + 2 * e + 2 * i - h - k) % 7;
    let m = (a + 11 * h + 22 * l) / 451;
    let month = (h + l - 7 * m + 114) / 31;
    let day = (h + l - 7 * m + 114) % 31 + 1;
    Date::from_ymd(year, month as u32, day as u32).expect("the computus gives a date")
}

/// A business calendar: a weekend and a set of holidays.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BusinessCalendar {
    /// A name, for reports.
    pub name: String,
    /// Weekend days as a bit mask: bit 0 is Monday, bit 6 Sunday.
    weekend: u8,
    /// Holidays, sorted and distinct.
    holidays: Vec<Date>,
}

impl Default for BusinessCalendar {
    fn default() -> Self {
        Self::weekends()
    }
}

impl BusinessCalendar {
    /// Saturdays and Sundays, and no holidays.
    #[must_use]
    pub fn weekends() -> Self {
        Self { name: "weekends".into(), weekend: 0b110_0000, holidays: Vec::new() }
    }

    /// A calendar of `weekend` days (0 is Monday) and `holidays`.
    ///
    /// # Errors
    /// A weekend day outside 0–6.
    pub fn new(name: impl Into<String>, weekend: &[u32], mut holidays: Vec<Date>) -> Result<Self> {
        let mut mask = 0u8;
        for &w in weekend {
            if w > 6 {
                return Err(Error::Invalid(format!("weekday {w} (0 is Monday, 6 Sunday)")));
            }
            mask |= 1 << w;
        }
        holidays.sort_unstable();
        holidays.dedup();
        Ok(Self { name: name.into(), weekend: mask, holidays })
    }

    /// A calendar of `weekend` days and the holidays `rules` make in `years` (inclusive).
    ///
    /// # Errors
    /// As [`BusinessCalendar::new`].
    pub fn from_rules(name: impl Into<String>, weekend: &[u32], rules: &[Rule], years: (i32, i32), extra: Vec<Date>) -> Result<Self> {
        let mut days = extra;
        for y in years.0..=years.1 {
            days.extend(rules.iter().filter_map(|r| r.in_year(y)));
        }
        Self::new(name, weekend, days)
    }

    /// Business days on both calendars: the union of their weekends and holidays.
    #[must_use]
    pub fn joint(cals: &[&Self]) -> Self {
        let mut out = Self { name: cals.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join("+"), weekend: 0, holidays: Vec::new() };
        for c in cals {
            out.weekend |= c.weekend;
            out.holidays.extend_from_slice(&c.holidays);
        }
        out.holidays.sort_unstable();
        out.holidays.dedup();
        out
    }

    /// The holidays declared.
    #[must_use]
    pub fn holidays(&self) -> &[Date] {
        &self.holidays
    }

    /// Whether `d` is a business day.
    #[must_use]
    pub fn is_business_day(&self, d: Date) -> bool {
        self.weekend & (1 << d.weekday()) == 0 && self.holidays.binary_search(&d).is_err()
    }

    /// `d`, moved by `conv` if it is not a business day.
    #[must_use]
    pub fn adjust(&self, d: Date, conv: BusinessDay) -> Date {
        if conv == BusinessDay::Unadjusted || self.is_business_day(d) {
            return d;
        }
        let step = |from: Date, by: i32| {
            let mut x = from.plus_days(by);
            while !self.is_business_day(x) {
                x = x.plus_days(by);
            }
            x
        };
        match conv {
            BusinessDay::Unadjusted => d,
            BusinessDay::Following => step(d, 1),
            BusinessDay::Preceding => step(d, -1),
            BusinessDay::ModifiedFollowing => {
                let f = step(d, 1);
                if f.month() == d.month() { f } else { step(d, -1) }
            }
            BusinessDay::ModifiedPreceding => {
                let p = step(d, -1);
                if p.month() == d.month() { p } else { step(d, 1) }
            }
        }
    }

    /// `n` business days after `d` (before it when negative). With `n = 0`, `d` itself if it is a
    /// business day, else the next one.
    #[must_use]
    pub fn add_business_days(&self, d: Date, n: i32) -> Date {
        let mut x = d;
        if n == 0 {
            return self.adjust(d, BusinessDay::Following);
        }
        let by = n.signum();
        let mut left = n.abs();
        while left > 0 {
            x = x.plus_days(by);
            if self.is_business_day(x) {
                left -= 1;
            }
        }
        x
    }

    /// Business days in [`d1`, `d2`).
    #[must_use]
    pub fn business_days_between(&self, d1: Date, d2: Date) -> i32 {
        let (a, b, sign) = if d1 <= d2 { (d1, d2, 1) } else { (d2, d1, -1) };
        let mut n = 0;
        let mut x = a;
        while x < b {
            if self.is_business_day(x) {
                n += 1;
            }
            x = x.plus_days(1);
        }
        sign * n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Date {
        s.parse().unwrap()
    }

    #[test]
    fn easter_is_computed() {
        assert_eq!(easter(2024), d("2024-03-31"));
        assert_eq!(easter(2025), d("2025-04-20"));
        assert_eq!(easter(2000), d("2000-04-23"));
        assert_eq!(easter(1961), d("1961-04-02"));
    }

    #[test]
    fn rules_make_holidays_and_observe_them() {
        let nearest = Rule::Fixed { month: 7, day: 4, observance: Observance::Nearest };
        assert_eq!(nearest.in_year(2026), Some(d("2026-07-03")), "a Saturday holiday on the Friday");
        assert_eq!(nearest.in_year(2027), Some(d("2027-07-05")), "a Sunday holiday on the Monday");
        assert_eq!(Rule::NthWeekday { month: 11, weekday: 3, n: 4 }.in_year(2026), Some(d("2026-11-26")), "the fourth Thursday");
        assert_eq!(Rule::NthWeekday { month: 5, weekday: 0, n: -1 }.in_year(2026), Some(d("2026-05-25")), "the last Monday");
        assert_eq!(Rule::Easter { offset: -2 }.in_year(2026), Some(d("2026-04-03")), "Good Friday");
    }

    #[test]
    fn dates_move_by_each_convention() {
        let cal = BusinessCalendar::new("x", &[5, 6], vec![d("2026-05-01"), d("2026-08-31")]).unwrap();
        // Saturday 30 May 2026: the following business day is Monday 1 June, in the next month.
        assert_eq!(cal.adjust(d("2026-05-30"), BusinessDay::Following), d("2026-06-01"));
        assert_eq!(cal.adjust(d("2026-05-30"), BusinessDay::ModifiedFollowing), d("2026-05-29"));
        assert_eq!(cal.adjust(d("2026-05-30"), BusinessDay::Preceding), d("2026-05-29"));
        // Friday 1 May 2026 is a holiday: the preceding day is in April, so modified preceding goes forward.
        assert_eq!(cal.adjust(d("2026-05-01"), BusinessDay::ModifiedPreceding), d("2026-05-04"));
        assert_eq!(cal.adjust(d("2026-05-01"), BusinessDay::Unadjusted), d("2026-05-01"));
        assert_eq!(cal.add_business_days(d("2026-08-28"), 1), d("2026-09-01"), "past the weekend and Monday's holiday");
        assert_eq!(cal.add_business_days(d("2026-09-01"), -1), d("2026-08-28"));
        assert_eq!(cal.business_days_between(d("2026-08-24"), d("2026-09-07")), 9);
        let joint = BusinessCalendar::joint(&[&cal, &BusinessCalendar::new("y", &[4], vec![d("2026-06-03")]).unwrap()]);
        assert!(!joint.is_business_day(d("2026-06-05")) && !joint.is_business_day(d("2026-06-03")) && joint.is_business_day(d("2026-06-04")));
    }
}
