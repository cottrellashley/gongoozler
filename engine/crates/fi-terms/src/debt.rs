//! A debt instrument's terms: its coupon leg, its principal and its rights, as data. One
//! definition covers bullets, amortisers, sinkers, floaters, linkers, zeros, callables, puttables
//! and soft bullets; nothing here is a type per product.

use crate::coupon::{Coupon, Fixings};
use crate::date::Date;
use crate::daycount::DayCount;
use crate::schedule::{Period, Schedule};
use crate::{Error, Result};

/// How principal is repaid before maturity.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Amortisation {
    /// All at maturity.
    #[default]
    Bullet,
    /// Face retired on each date, per unit of original face, at a price per 100 (a sinking fund
    /// at par is 100). The rest is repaid at maturity.
    Schedule(Vec<(Date, f64, f64)>),
}

/// Who holds an option on the principal.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum RightKind {
    /// The issuer may repay early.
    Call,
    /// The holder may demand early repayment.
    Put,
}

/// A right to repay (call) or be repaid (put) on dates in a window, at a price per 100. A
/// make-whole call pays the larger of the price and the remaining flows discounted at a curve plus
/// `make_whole_bp`.
#[derive(Clone, Debug, PartialEq)]
pub struct Right {
    /// Call or put.
    pub kind: RightKind,
    /// First date it may be exercised.
    pub from: Date,
    /// Last date it may be exercised.
    pub to: Date,
    /// Price per 100 of current face, before accrued interest.
    pub price: f64,
    /// Business days of notice the exercise needs.
    pub notice: i32,
    /// The make-whole spread over the reference curve, in basis points, if the call is make-whole.
    pub make_whole_bp: Option<f64>,
}

/// A soft bullet: unpaid at maturity, the debt extends to `to`, at its own coupon if given.
#[derive(Clone, Debug, PartialEq)]
pub struct Extension {
    /// The extended maturity.
    pub to: Date,
    /// The coupon over the extension (default: the original).
    pub coupon: Option<Coupon>,
}

/// A debt instrument's terms.
#[derive(Clone, Debug, PartialEq)]
pub struct Debt {
    /// The currency it pays in (an ISO code, for reports and rounding).
    pub currency: String,
    /// The coupon periods.
    pub schedule: Schedule,
    /// How coupons accrue.
    pub day_count: DayCount,
    /// How each period's rate is set.
    pub coupon: Coupon,
    /// Redemption price per 100 at maturity.
    pub redemption: f64,
    /// Repayment before maturity.
    pub amortisation: Amortisation,
    /// Calls and puts.
    pub rights: Vec<Right>,
    /// A soft-bullet extension.
    pub extension: Option<Extension>,
    /// Business days from a trade to its settlement.
    pub settle_days: i32,
    /// Decimal places each contractual amount per unit of original face is rounded to, half away
    /// from zero (5: cents per 1,000 of face); `None`, unrounded.
    pub rounding: Option<u32>,
}

/// What a flow is.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum CashflowKind {
    /// Interest for a period.
    Coupon,
    /// Principal before maturity (amortisation, a sinking fund).
    Principal,
    /// Principal at maturity.
    Redemption,
}

/// One contractual flow, per unit of original face.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Cashflow {
    /// What it is.
    pub kind: CashflowKind,
    /// When it is scheduled, before any business-day adjustment (what a yield counts to).
    pub date: Date,
    /// When it pays.
    pub pay: Date,
    /// When it goes ex (coupons), if before payment.
    pub ex: Option<Date>,
    /// The accrual period (coupons).
    pub accrual: Option<(Date, Date)>,
    /// The year fraction (coupons).
    pub fraction: f64,
    /// The rate per year (coupons).
    pub rate: f64,
    /// The amount, per unit of original face.
    pub amount: f64,
    /// Face retired by it (principal and redemption), per unit of original face.
    pub face: f64,
}

/// A price, per 100 of current face.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Price {
    /// Before accrued interest.
    pub clean: f64,
    /// Accrued interest.
    pub accrued: f64,
    /// Clean plus accrued.
    pub dirty: f64,
}

impl Debt {
    /// A fixed-rate bullet: `rate` a year, `per_year` coupons, `day_count`, from `issue` to
    /// `maturity`, settling two business days after a trade.
    ///
    /// # Errors
    /// Coupons a year that do not divide twelve.
    pub fn fixed(currency: &str, issue: Date, maturity: Date, rate: f64, per_year: u32, day_count: DayCount) -> Result<Self> {
        if per_year != 0 && !12u32.is_multiple_of(per_year) {
            return Err(Error::Invalid(format!("{per_year} coupons a year do not divide twelve months")));
        }
        Ok(Self {
            currency: currency.into(),
            schedule: Schedule::new(issue, maturity, 12u32.checked_div(per_year).unwrap_or(0)),
            day_count,
            coupon: if rate == 0.0 && per_year == 0 { Coupon::Zero } else { Coupon::Fixed { rate } },
            redemption: 100.0,
            amortisation: Amortisation::Bullet,
            rights: Vec::new(),
            extension: None,
            settle_days: 2,
            rounding: None,
        })
    }

    /// `x` (an amount per unit of original face) rounded as the terms say. Float noise within a
    /// relative 1e-12 of a decimal half counts as the half.
    #[must_use]
    pub fn round(&self, x: f64) -> f64 {
        let Some(dp) = self.rounding else { return x };
        if !x.is_finite() {
            return x;
        }
        let f = 10f64.powi(dp as i32);
        let q = x.abs() * f;
        let tol = q.max(1.0) * 1e-12;
        let n = if (q - q.floor() - 0.5).abs() <= tol { q.floor() + 1.0 } else { q.round() };
        (n / f).copysign(x)
    }

    /// The first accrual date.
    #[must_use]
    pub fn issue(&self) -> Date {
        self.schedule.start
    }

    /// The scheduled maturity.
    #[must_use]
    pub fn maturity(&self) -> Date {
        self.schedule.end
    }

    /// The coupon periods.
    ///
    /// # Errors
    /// Inconsistent schedule terms.
    pub fn periods(&self) -> Result<Vec<Period>> {
        self.schedule.periods()
    }

    /// Face outstanding per unit of original face at the end of `date`: one, less what the
    /// amortisation schedule retired on or before it.
    #[must_use]
    pub fn balance(&self, date: Date) -> f64 {
        if date >= self.maturity() { 0.0 } else { self.outstanding_from(date) }
    }

    /// The year fraction of `d1` to `d2` within `period`.
    ///
    /// # Errors
    /// As [`DayCount::fraction`].
    pub fn fraction(&self, d1: Date, d2: Date, period: &Period) -> Result<f64> {
        self.day_count.fraction(d1, d2, Some(period.reference), Some(self.maturity()))
    }

    /// Interest from `period.start` to `date` (within the period), per unit of original face, on
    /// the balance outstanding over each part of it.
    ///
    /// # Errors
    /// A fixing the period needs and does not have.
    pub fn interest(&self, period: &Period, date: Date, fixings: &dyn Fixings) -> Result<f64> {
        let rate = self.coupon.rate(period, |a, b| self.day_count.fraction(a, b, Some(period.reference), Some(self.maturity())).unwrap_or(0.0), fixings)?;
        let end = date.min(period.end);
        if end <= period.start {
            return Ok(0.0);
        }
        // The balance changes only on amortisation dates: accrue piece by piece between them.
        let mut cuts: Vec<Date> = match &self.amortisation {
            Amortisation::Schedule(s) => s.iter().map(|(d, _, _)| *d).filter(|d| *d > period.start && *d < end).collect(),
            Amortisation::Bullet => Vec::new(),
        };
        cuts.push(end);
        let mut total = 0.0;
        let mut from = period.start;
        for to in cuts {
            total += self.outstanding_from(from) * rate * self.fraction(from, to, period)?;
            from = to;
        }
        Ok(total * self.coupon.index_ratio(end, fixings)?)
    }

    /// Face accruing from `date` on, per unit of original face: one, less the amortisation on or
    /// before it.
    fn outstanding_from(&self, date: Date) -> f64 {
        match &self.amortisation {
            Amortisation::Schedule(s) => (1.0 - s.iter().filter(|(d, _, _)| *d <= date).map(|(_, f, _)| f).sum::<f64>()).max(0.0),
            Amortisation::Bullet => 1.0,
        }
    }

    /// The period accruing on `date` (the one whose start is on or before it and whose end is
    /// after it).
    ///
    /// # Errors
    /// Inconsistent schedule terms.
    pub fn period_at(&self, date: Date) -> Result<Option<Period>> {
        Ok(self.periods()?.into_iter().find(|p| p.start <= date && date < p.end))
    }

    /// Accrued interest on `date`, per unit of original face. Once a coupon has gone ex it is
    /// negative: the interest from `date` to the period's end, which the buyer will not receive.
    ///
    /// # Errors
    /// A fixing the period needs and does not have.
    pub fn accrued(&self, date: Date, fixings: &dyn Fixings) -> Result<f64> {
        let Some(p) = self.period_at(date)? else { return Ok(0.0) };
        let to_date = self.interest(&p, date, fixings)?;
        if p.ex.is_some_and(|ex| date >= ex) {
            return Ok(to_date - self.interest(&p, p.end, fixings)?);
        }
        Ok(to_date)
    }

    /// Every contractual flow, in order of payment, per unit of original face: coupons on the
    /// balance outstanding, principal on the amortisation dates and the rest at maturity (indexed
    /// for an inflation-linked bond, and floored at par if its terms say so).
    ///
    /// # Errors
    /// A fixing a period needs and does not have.
    pub fn cashflows(&self, fixings: &dyn Fixings) -> Result<Vec<Cashflow>> {
        let periods = self.periods()?;
        let mut out = Vec::with_capacity(periods.len() + 2);
        for p in &periods {
            if !matches!(self.coupon, Coupon::Zero) {
                let amount = self.round(self.interest(p, p.end, fixings)?);
                let rate = self.coupon.rate(p, |a, b| self.day_count.fraction(a, b, Some(p.reference), Some(self.maturity())).unwrap_or(0.0), fixings)?;
                out.push(Cashflow {
                    kind: CashflowKind::Coupon,
                    date: p.end,
                    pay: p.pay,
                    ex: p.ex,
                    accrual: Some((p.start, p.end)),
                    fraction: self.fraction(p.start, p.end, p)?,
                    rate,
                    amount,
                    face: 0.0,
                });
            }
        }
        let cal = &self.schedule.calendar;
        let mut retired = 0.0;
        if let Amortisation::Schedule(s) = &self.amortisation {
            for &(d, face, price) in s {
                if d > self.issue() && d < self.maturity() {
                    let ratio = self.coupon.index_ratio(d, fixings)?;
                    let pay = cal.adjust(d, self.schedule.adjust);
                    out.push(Cashflow {
                        kind: CashflowKind::Principal,
                        date: d,
                        pay,
                        ex: None,
                        accrual: None,
                        fraction: 0.0,
                        rate: 0.0,
                        amount: self.round(face * price / 100.0 * ratio),
                        face,
                    });
                    retired += face;
                }
            }
        }
        let remaining = (1.0 - retired).max(0.0);
        let mut ratio = self.coupon.index_ratio(self.maturity(), fixings)?;
        if self.coupon.floors_principal() {
            ratio = ratio.max(1.0);
        }
        let pay = cal.adjust(self.maturity(), self.schedule.adjust);
        out.push(Cashflow {
            kind: CashflowKind::Redemption,
            date: self.maturity(),
            pay,
            ex: None,
            accrual: None,
            fraction: 0.0,
            rate: 0.0,
            amount: self.round(remaining * self.redemption / 100.0 * ratio),
            face: remaining,
        });
        out.sort_by_key(|c| (c.pay, c.kind as u8));
        Ok(out)
    }

    /// The right of `kind` that may be exercised on `date`, if any.
    #[must_use]
    pub fn right_on(&self, kind: RightKind, date: Date) -> Option<&Right> {
        self.rights.iter().find(|r| r.kind == kind && r.from <= date && date <= r.to)
    }

    /// The settlement date of a trade on `trade`.
    #[must_use]
    pub fn settlement(&self, trade: Date) -> Date {
        self.schedule.calendar.add_business_days(trade, self.settle_days)
    }

    /// Checks the terms hang together.
    ///
    /// # Errors
    /// Amortisation beyond the face, a redemption that is not positive, a right outside the
    /// instrument's life, an extension before maturity.
    pub fn validate(&self) -> Result<()> {
        self.periods()?;
        if !(self.redemption.is_finite() && self.redemption > 0.0) {
            return Err(Error::Invalid(format!("redemption {} per 100", self.redemption)));
        }
        if let Amortisation::Schedule(s) = &self.amortisation {
            let total: f64 = s.iter().map(|(_, f, _)| f).sum();
            if total > 1.0 + 1e-12 || s.iter().any(|(_, f, p)| *f < 0.0 || !p.is_finite()) {
                return Err(Error::Invalid(format!("the amortisation retires {total} of the face")));
            }
        }
        for r in &self.rights {
            if r.to < r.from || r.from < self.issue() || r.from > self.maturity() {
                return Err(Error::Invalid(format!("a {:?} from {} to {} outside the life of the debt", r.kind, r.from, r.to)));
            }
        }
        if let Some(e) = &self.extension
            && e.to <= self.maturity()
        {
            return Err(Error::Invalid(format!("an extension to {} is not after the maturity {}", e.to, self.maturity())));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coupon::NoFixings;

    fn d(s: &str) -> Date {
        s.parse().unwrap()
    }

    #[test]
    fn amounts_round_as_the_terms_say() {
        let mut b = Debt::fixed("USD", d("2024-01-02"), d("2024-07-02"), 0.06, 4, DayCount::Act360).unwrap();
        b.schedule.adjust = crate::BusinessDay::Unadjusted;
        let raw: Vec<f64> = b.cashflows(&NoFixings).unwrap().iter().map(|c| c.amount).collect();
        assert!((raw[0] - 0.06 * 91.0 / 360.0).abs() < 1e-17);
        b.rounding = Some(7);
        let rounded: Vec<f64> = b.cashflows(&NoFixings).unwrap().iter().map(|c| c.amount).collect();
        assert_eq!(rounded, vec![0.0151667, 0.0151667, 1.0]);
        b.rounding = Some(2);
        assert_eq!(b.round(0.125), 0.13, "a decimal half rounds away from zero");
        assert_eq!(b.round(-0.125), -0.13);
        assert_eq!(b.round(1.005), 1.01, "though binary holds 1.005 as 1.00499999…");
    }

    #[test]
    fn a_fixed_bullet_pays_its_coupons_and_its_face() {
        let b = Debt::fixed("USD", d("2024-01-15"), d("2026-01-15"), 0.05, 2, DayCount::Thirty360).unwrap();
        let flows = b.cashflows(&NoFixings).unwrap();
        assert_eq!(flows.len(), 5);
        assert!(flows[..4].iter().all(|c| c.kind == CashflowKind::Coupon && (c.amount - 0.025).abs() < 1e-15));
        assert_eq!(flows[4].kind, CashflowKind::Redemption);
        assert!((flows[4].amount - 1.0).abs() < 1e-15);
        // 15 Jul 2025 is a Tuesday; 15 Jan 2026 a Thursday: no adjustment.
        assert_eq!(flows[3].pay, d("2026-01-15"));
    }

    #[test]
    fn accrued_interest_follows_the_day_count_and_goes_negative_ex_coupon() {
        let mut b = Debt::fixed("GBP", d("2025-03-07"), d("2030-03-07"), 0.04, 2, DayCount::ActActIcma).unwrap();
        // 7 Mar to 7 Sep 2025 is 184 days; 30 days in, ICMA accrues 30 / (2 × 184) of the year.
        let want = 0.04 * 30.0 / (2.0 * 184.0);
        assert!((b.accrued(d("2025-04-06"), &NoFixings).unwrap() - want).abs() < 1e-15);
        b.schedule.ex_days = 7;
        // The coupon of 7 Sep 2025 (a Sunday, paid Monday 8 Sep) goes ex seven business days before
        // the payment: Thursday 28 Aug. On 1 Sep, a buyer owes back the six days to 7 Sep.
        let ex = b.accrued(d("2025-09-01"), &NoFixings).unwrap();
        assert!((ex + 0.04 * 6.0 / (2.0 * 184.0)).abs() < 1e-15, "{ex}");
    }

    #[test]
    fn an_amortiser_pays_coupons_on_what_is_outstanding() {
        let mut b = Debt::fixed("USD", d("2024-01-01"), d("2026-01-01"), 0.06, 1, DayCount::Thirty360).unwrap();
        b.amortisation = Amortisation::Schedule(vec![(d("2025-01-01"), 0.4, 100.0)]);
        b.validate().unwrap();
        let flows = b.cashflows(&NoFixings).unwrap();
        let coupons: Vec<f64> = flows.iter().filter(|c| c.kind == CashflowKind::Coupon).map(|c| c.amount).collect();
        assert!((coupons[0] - 0.06).abs() < 1e-15 && (coupons[1] - 0.6 * 0.06).abs() < 1e-15, "{coupons:?}");
        let principal: Vec<f64> = flows.iter().filter(|c| c.kind != CashflowKind::Coupon).map(|c| c.amount).collect();
        assert!((principal[0] - 0.4).abs() < 1e-15 && (principal[1] - 0.6).abs() < 1e-15);
        assert!((b.balance(d("2025-06-01")) - 0.6).abs() < 1e-15 && b.balance(d("2026-01-01")) == 0.0);
        b.amortisation = Amortisation::Schedule(vec![(d("2025-01-01"), 1.4, 100.0)]);
        assert!(b.validate().is_err(), "more than the face");
    }

    #[test]
    fn rights_and_extensions_are_checked_against_the_life_of_the_debt() {
        let mut b = Debt::fixed("EUR", d("2024-01-01"), d("2030-01-01"), 0.03, 1, DayCount::ActActIcma).unwrap();
        b.rights.push(Right { kind: RightKind::Call, from: d("2027-01-01"), to: d("2029-12-31"), price: 101.0, notice: 30, make_whole_bp: None });
        assert!(b.right_on(RightKind::Call, d("2028-06-01")).is_some() && b.right_on(RightKind::Call, d("2026-06-01")).is_none());
        b.extension = Some(Extension { to: d("2029-01-01"), coupon: None });
        assert!(b.validate().is_err());
        b.extension = Some(Extension { to: d("2031-01-01"), coupon: None });
        b.validate().unwrap();
        assert_eq!(b.settlement(d("2026-11-06")), d("2026-11-10"), "two business days after a Friday");
    }
}
