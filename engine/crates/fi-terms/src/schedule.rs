//! Coupon schedules: the accrual periods of a leg, when each pays, and when it goes ex.

use crate::calendar::{BusinessCalendar, BusinessDay};
use crate::date::Date;
use crate::daycount::Reference;
use crate::{Error, Result};

/// Where an irregular period goes, when the dates do not divide into whole periods.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum Stub {
    /// Dates roll back from the end; the first period is short (bonds' usual case).
    #[default]
    ShortFront,
    /// Dates roll back from the end; a short first period joins the next.
    LongFront,
    /// Dates roll forward from the start; the last period is short.
    ShortBack,
    /// Dates roll forward from the start; a short last period joins the one before.
    LongBack,
}

/// How a leg's periods are made.
#[derive(Clone, Debug, PartialEq)]
pub struct Schedule {
    /// The first accrual date (issue, or dated date).
    pub start: Date,
    /// The last accrual date (maturity).
    pub end: Date,
    /// Months per period; 0 makes one period, start to end (a zero coupon or a money-market
    /// instrument).
    pub months: u32,
    /// Where an irregular period goes.
    pub stub: Stub,
    /// The first regular coupon date, if the first period is irregular by the terms.
    pub first: Option<Date>,
    /// The last regular coupon date before the end, if the last period is irregular by the terms.
    pub last: Option<Date>,
    /// With a month-end start (or end), roll every date to its month's end.
    pub eom: bool,
    /// The business days payments are made on.
    pub calendar: BusinessCalendar,
    /// How a payment date that is not a business day moves.
    pub adjust: BusinessDay,
    /// Whether accrual dates move with their payments (swaps), or stay as scheduled (bonds).
    pub adjust_accrual: bool,
    /// Business days after the period's end that it pays.
    pub pay_lag: i32,
    /// Business days before the payment date the coupon goes ex (0: never ex before payment).
    pub ex_days: i32,
    /// An explicit list of period ends, instead of rolling by months.
    pub dates: Option<Vec<Date>>,
}

impl Schedule {
    /// A schedule from `start` to `end`, a period every `months`, rolled back from the end, paid
    /// on the scheduled day moved to the next business day of a Saturday–Sunday calendar.
    #[must_use]
    pub fn new(start: Date, end: Date, months: u32) -> Self {
        Self {
            start,
            end,
            months,
            stub: Stub::ShortFront,
            first: None,
            last: None,
            eom: false,
            calendar: BusinessCalendar::weekends(),
            adjust: BusinessDay::Following,
            adjust_accrual: false,
            pay_lag: 0,
            ex_days: 0,
            dates: None,
        }
    }

    /// Coupons a year (0 for a single period).
    #[must_use]
    pub fn per_year(&self) -> u32 {
        12u32.checked_div(self.months).unwrap_or(0)
    }

    /// The periods, in order.
    ///
    /// # Errors
    /// An end not after the start, a period that does not divide a year, or explicit dates that
    /// are not increasing.
    pub fn periods(&self) -> Result<Vec<Period>> {
        if self.end <= self.start {
            return Err(Error::Invalid(format!("a schedule ending {} on or before it starts {}", self.end, self.start)));
        }
        if self.months > 0 && !12u32.is_multiple_of(self.months) && !self.months.is_multiple_of(12) {
            return Err(Error::Invalid(format!("a period of {} months does not divide a year", self.months)));
        }
        let ends = self.ends()?;
        let mut out = Vec::with_capacity(ends.len());
        let mut start = self.start;
        let n = ends.len();
        for (i, &end) in ends.iter().enumerate() {
            let regular = self.months > 0 && self.is_regular(start, end);
            let (ref_start, ref_end) = if regular || self.months == 0 {
                (start, end)
            } else if i == 0 {
                // A front stub is measured against the regular period ending where it ends.
                (end.plus_months(-(self.months as i32), self.eom), end)
            } else {
                (start, start.plus_months(self.months as i32, self.eom))
            };
            let pay_base = self.calendar.adjust(end, self.adjust);
            let pay = if self.pay_lag == 0 { pay_base } else { self.calendar.add_business_days(pay_base, self.pay_lag) };
            let ex = (self.ex_days > 0).then(|| self.calendar.add_business_days(pay, -self.ex_days));
            let (acc_start, acc_end) = if self.adjust_accrual {
                (if i == 0 { start } else { self.calendar.adjust(start, self.adjust) }, if i + 1 == n { end } else { self.calendar.adjust(end, self.adjust) })
            } else {
                (start, end)
            };
            out.push(Period {
                start: acc_start,
                end: acc_end,
                pay,
                ex,
                reference: Reference { start: ref_start, end: ref_end, per_year: self.per_year().max(1) },
                regular,
            });
            start = end;
        }
        Ok(out)
    }

    fn is_regular(&self, start: Date, end: Date) -> bool {
        let m = self.months as i32;
        start.plus_months(m, self.eom) == end || (self.eom && start.is_month_end() && end == start.plus_months(m, true))
    }

    /// The unadjusted period ends, strictly after the start, the last being the end.
    fn ends(&self) -> Result<Vec<Date>> {
        if let Some(dates) = &self.dates {
            let mut v: Vec<Date> = dates.iter().copied().filter(|&d| d > self.start && d < self.end).collect();
            if v.windows(2).any(|w| w[0] >= w[1]) {
                return Err(Error::Invalid("explicit schedule dates must increase".into()));
            }
            v.push(self.end);
            return Ok(v);
        }
        if self.months == 0 {
            return Ok(vec![self.end]);
        }
        let m = self.months as i32;
        let eom = self.eom && (self.end.is_month_end() || self.start.is_month_end());
        let mut ends = Vec::new();
        match self.stub {
            Stub::ShortFront | Stub::LongFront => {
                let anchor_end = self.last.unwrap_or(self.end);
                let floor = self.first.unwrap_or(self.start);
                let mut k = 0;
                loop {
                    let d = anchor_end.plus_months(-k * m, eom);
                    if d <= floor {
                        break;
                    }
                    ends.push(d);
                    k += 1;
                }
                if let Some(f) = self.first.filter(|&f| f > self.start) {
                    ends.push(f);
                }
                ends.reverse();
                if self.last.is_some_and(|l| l < self.end) {
                    ends.push(self.end);
                }
                if self.stub == Stub::LongFront && self.first.is_none() && ends.len() >= 2 {
                    // The first period would be short: join it to the second.
                    let first_start = self.start;
                    if first_start.plus_months(m, eom) != ends[0] {
                        ends.remove(0);
                    }
                }
            }
            Stub::ShortBack | Stub::LongBack => {
                let anchor = self.first.unwrap_or(self.start);
                if self.first.is_some_and(|f| f > self.start) {
                    ends.push(anchor);
                }
                let ceiling = self.last.unwrap_or(self.end);
                let mut k = 1;
                loop {
                    let d = anchor.plus_months(k * m, eom);
                    if d >= ceiling {
                        break;
                    }
                    ends.push(d);
                    k += 1;
                }
                if self.last.is_some_and(|l| l < self.end) {
                    ends.push(self.last.expect("checked"));
                }
                if self.stub == Stub::LongBack && self.last.is_none() && !ends.is_empty() {
                    let last_start = *ends.last().expect("not empty");
                    if last_start.plus_months(m, eom) != self.end {
                        ends.pop();
                    }
                }
                ends.push(self.end);
            }
        }
        ends.dedup();
        Ok(ends)
    }
}

/// One accrual period.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Period {
    /// Accrual starts (inclusive).
    pub start: Date,
    /// Accrual ends (exclusive): the next period's start.
    pub end: Date,
    /// When it pays.
    pub pay: Date,
    /// When it goes ex: a buyer after this date does not receive it.
    pub ex: Option<Date>,
    /// The period ACT/ACT ICMA measures it against (a notional one for a stub).
    pub reference: Reference,
    /// Whether it is a whole regular period.
    pub regular: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Date {
        s.parse().unwrap()
    }

    fn ends(s: &Schedule) -> Vec<String> {
        s.periods().unwrap().iter().map(|p| p.end.to_string()).collect()
    }

    #[test]
    fn a_regular_semiannual_bond_rolls_back_from_maturity() {
        let s = Schedule::new(d("2024-01-15"), d("2027-01-15"), 6);
        assert_eq!(ends(&s), ["2024-07-15", "2025-01-15", "2025-07-15", "2026-01-15", "2026-07-15", "2027-01-15"]);
        assert!(s.periods().unwrap().iter().all(|p| p.regular));
    }

    #[test]
    fn stubs_go_where_the_terms_say() {
        // Issued 1 Mar 2024, maturing 15 Jan 2027, semiannual: a short first period to 15 Jul 2024.
        let mut s = Schedule::new(d("2024-03-01"), d("2027-01-15"), 6);
        let p = s.periods().unwrap();
        assert_eq!(p[0].end, d("2024-07-15"));
        assert!(!p[0].regular && p[1].regular);
        assert_eq!((p[0].reference.start, p[0].reference.end), (d("2024-01-15"), d("2024-07-15")), "measured against its notional period");
        s.stub = Stub::LongFront;
        assert_eq!(s.periods().unwrap()[0].end, d("2025-01-15"), "the short period joins the next");
        s.stub = Stub::ShortBack;
        assert_eq!(ends(&s), ["2024-09-01", "2025-03-01", "2025-09-01", "2026-03-01", "2026-09-01", "2027-01-15"]);
        s.stub = Stub::LongBack;
        assert_eq!(ends(&s).last().unwrap(), "2027-01-15");
        assert_eq!(ends(&s).len(), 5, "the short last period joins the one before");
    }

    #[test]
    fn the_end_of_month_rule_keeps_month_ends() {
        let mut s = Schedule::new(d("2024-02-29"), d("2025-08-31"), 6);
        s.eom = true;
        assert_eq!(ends(&s), ["2024-08-31", "2025-02-28", "2025-08-31"]);
    }

    #[test]
    fn payments_move_to_business_days_and_go_ex_before_them() {
        let mut s = Schedule::new(d("2025-03-15"), d("2026-03-15"), 6);
        s.ex_days = 7;
        let p = s.periods().unwrap();
        // 15 Mar 2026 is a Sunday: paid Monday 16 Mar; ex seven business days before.
        assert_eq!(p[1].pay, d("2026-03-16"));
        assert_eq!(p[1].ex, Some(d("2026-03-05")));
        assert_eq!(p[1].end, d("2026-03-15"), "a bond's accrual dates are not moved");
        s.pay_lag = 2;
        assert_eq!(s.periods().unwrap()[1].pay, d("2026-03-18"));
    }

    #[test]
    fn a_zero_coupon_has_one_period_and_bad_terms_are_refused() {
        assert_eq!(ends(&Schedule::new(d("2025-01-01"), d("2030-01-01"), 0)), ["2030-01-01"]);
        assert!(Schedule::new(d("2025-01-01"), d("2024-01-01"), 6).periods().is_err());
        assert!(Schedule::new(d("2025-01-01"), d("2026-01-01"), 5).periods().is_err());
        let mut s = Schedule::new(d("2025-01-01"), d("2026-01-01"), 3);
        s.dates = Some(vec![d("2025-05-20"), d("2025-09-01")]);
        assert_eq!(ends(&s), ["2025-05-20", "2025-09-01", "2026-01-01"]);
    }
}
