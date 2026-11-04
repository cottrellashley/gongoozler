//! Time: nanoseconds since the Unix epoch, and the economic calendar.

use std::fmt;

use crate::hash::Hash64;

/// A time: nanoseconds since the Unix epoch, UTC. The same unit as Nautilus's `UnixNanos`, but signed.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ts(pub i64);

impl Ts {
    /// Nanoseconds in one day.
    pub const DAY: i64 = 86_400_000_000_000;
    /// The earliest time.
    pub const MIN: Ts = Ts(i64::MIN);
    /// The latest time: "known by the end of everything".
    pub const MAX: Ts = Ts(i64::MAX);

    /// Midnight UTC of `days` days since 1970-01-01.
    #[must_use]
    pub const fn from_days(days: i64) -> Self {
        Ts(days * Self::DAY)
    }

    /// Whole days since 1970-01-01 (floor).
    #[must_use]
    pub const fn days(self) -> i64 {
        self.0.div_euclid(Self::DAY)
    }

    /// `self` plus `nanos`, saturating.
    #[must_use]
    pub const fn plus(self, nanos: i64) -> Self {
        Ts(self.0.saturating_add(nanos))
    }

    /// Actual/365F year fraction from `self` to `to`.
    #[must_use]
    pub fn years_to(self, to: Ts) -> f64 {
        (to.0 - self.0) as f64 / (365.0 * Self::DAY as f64)
    }

    /// Calendar date (year, month, day) of this time, UTC.
    #[must_use]
    pub fn ymd(self) -> (i32, u32, u32) {
        civil_from_days(self.days())
    }
}

/// A date at midnight UTC as `YYYY-MM-DD`; any other instant as `YYYY-MM-DDTHH:MM:SSZ` (with
/// nanoseconds if it has them).
impl fmt::Display for Ts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (y, m, d) = self.ymd();
        write!(f, "{y:04}-{m:02}-{d:02}")?;
        let ns = self.0.rem_euclid(Self::DAY);
        if ns != 0 {
            let s = ns / 1_000_000_000;
            write!(f, "T{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)?;
            if ns % 1_000_000_000 != 0 {
                write!(f, ".{:09}", ns % 1_000_000_000)?;
            }
            f.write_str("Z")?;
        }
        Ok(())
    }
}

/// Days since 1970-01-01 to (year, month, day): Howard Hinnant's `civil_from_days`.
#[must_use]
pub fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
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
#[must_use]
pub fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = i64::from(y) - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The economic clock: the sorted, distinct times positions are valued at.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Calendar {
    times: Vec<Ts>,
}

impl Calendar {
    /// A calendar from any times: sorted and deduplicated.
    #[must_use]
    pub fn new(mut times: Vec<Ts>) -> Self {
        times.sort_unstable();
        times.dedup();
        Self { times }
    }

    /// Weekdays (Monday to Friday) from `start` to `end` inclusive, at midnight UTC.
    #[must_use]
    pub fn weekdays(start: Ts, end: Ts) -> Self {
        let mut times = Vec::new();
        let mut d = start.days();
        while d <= end.days() {
            // 1970-01-01 was a Thursday: (d + 3) mod 7 is 0 on Mondays.
            if (d + 3).rem_euclid(7) < 5 {
                times.push(Ts::from_days(d));
            }
            d += 1;
        }
        Self { times }
    }

    /// The times, ascending.
    #[must_use]
    pub fn times(&self) -> &[Ts] {
        &self.times
    }

    /// Number of times.
    #[must_use]
    pub fn len(&self) -> usize {
        self.times.len()
    }

    /// Whether there are no times.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.times.is_empty()
    }

    /// Position of `t`, if it is on the calendar.
    #[must_use]
    pub fn index(&self, t: Ts) -> Option<usize> {
        self.times.binary_search(&t).ok()
    }

    /// The first `n` times (a prefix): used to check that batch and step evaluation agree.
    #[must_use]
    pub fn prefix(&self, n: usize) -> Self {
        Self { times: self.times[..n.min(self.times.len())].to_vec() }
    }

    /// The union of several calendars (the data's own dates, as the legacy frameworks use).
    #[must_use]
    pub fn union(cals: &[&Calendar]) -> Self {
        Self::new(cals.iter().flat_map(|c| c.times.iter().copied()).collect())
    }

    /// The times every calendar has.
    #[must_use]
    pub fn intersection(cals: &[&Calendar]) -> Self {
        let Some((first, rest)) = cals.split_first() else {
            return Self::default();
        };
        let times = first.times.iter().copied().filter(|t| rest.iter().all(|c| c.index(*t).is_some())).collect();
        Self { times }
    }

    /// The last time of each calendar month: a month-end rebalance schedule.
    #[must_use]
    pub fn month_ends(&self) -> Vec<Ts> {
        let mut out: Vec<Ts> = Vec::new();
        for (i, t) in self.times.iter().enumerate() {
            let (y, m, _) = t.ymd();
            let last = match self.times.get(i + 1) {
                Some(next) => {
                    let (ny, nm, _) = next.ymd();
                    (ny, nm) != (y, m)
                }
                None => true,
            };
            if last {
                out.push(*t);
            }
        }
        out
    }

    /// Every `step`-th time starting at `offset`: a fixed-frequency schedule.
    #[must_use]
    pub fn every(&self, step: usize, offset: usize) -> Vec<Ts> {
        self.times.iter().copied().skip(offset).step_by(step.max(1)).collect()
    }

    /// A content hash, for cache keys.
    #[must_use]
    pub fn version(&self) -> u64 {
        let mut h = Hash64::new();
        h.str("calendar");
        for t in &self.times {
            h.i64(t.0);
        }
        h.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_round_trip() {
        for d in [-1_000_000i64, -1, 0, 1, 10_957, 19_000, 2_000_000] {
            let (y, m, dd) = civil_from_days(d);
            assert_eq!(days_from_civil(y, m, dd), d);
        }
        assert_eq!(Ts::from_days(days_from_civil(2024, 2, 29)).to_string(), "2024-02-29");
    }

    #[test]
    fn weekdays_and_month_ends() {
        let start = Ts::from_days(days_from_civil(2024, 1, 1));
        let end = Ts::from_days(days_from_civil(2024, 3, 31));
        let cal = Calendar::weekdays(start, end);
        assert!(cal.times().iter().all(|t| (t.days() + 3).rem_euclid(7) < 5));
        let me: Vec<String> = cal.month_ends().iter().map(ToString::to_string).collect();
        assert_eq!(me, ["2024-01-31", "2024-02-29", "2024-03-29"]);
    }

    #[test]
    fn union_and_intersection() {
        let a = Calendar::new(vec![Ts(1), Ts(2), Ts(3)]);
        let b = Calendar::new(vec![Ts(2), Ts(3), Ts(4)]);
        assert_eq!(Calendar::union(&[&a, &b]).times(), &[Ts(1), Ts(2), Ts(3), Ts(4)]);
        assert_eq!(Calendar::intersection(&[&a, &b]).times(), &[Ts(2), Ts(3)]);
    }
}
