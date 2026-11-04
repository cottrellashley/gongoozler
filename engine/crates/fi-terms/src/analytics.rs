//! Prices, yields, spreads and sensitivities of a debt instrument, from its terms.
//!
//! Every price is per 100 of the face outstanding at settlement. A sensitivity carries the
//! definition of its bump (review C3, the brief's §5): what moved, by how much, in what units,
//! and that nothing was recalibrated.

use crate::coupon::Fixings;
use crate::curve::Curve;
use crate::date::Date;
use crate::debt::{Cashflow, CashflowKind, Debt, Price};
use crate::{Error, Result};

/// How a yield discounts.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum YieldConvention {
    /// Compounded at the coupon frequency over whole and fractional coupon periods (ICMA's
    /// convention, and the street's for most government and corporate bonds).
    #[default]
    Street,
    /// Compounded annually over ACT/365F years.
    Annual,
    /// Continuously compounded over ACT/365F years.
    Continuous,
}

/// What a sensitivity moves.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum BumpKind {
    /// Every zero rate of the discount curve.
    Parallel,
    /// One point of the discount curve (a triangular key-rate bump under linear zero interpolation).
    KeyRate {
        /// The point's index.
        point: usize,
    },
    /// The spread over the curve.
    Spread,
}

/// A bump: what moves, and by how many basis points either way.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Bump {
    /// What moves.
    pub kind: BumpKind,
    /// The size of each side of the central difference, in basis points.
    pub size_bp: f64,
}

/// A sensitivity with its definition.
#[derive(Clone, Debug, PartialEq)]
pub struct Sensitivity {
    /// The bump.
    pub bump: Bump,
    /// The change in dirty price per 100 of current face for a one-basis-point *fall* (positive
    /// for a long position in a bond).
    pub value: f64,
    /// What exactly was measured.
    pub definition: String,
}

/// The flows still to come to a buyer settling on `settle`: those paying after it, except a coupon
/// that went ex on or before it (it belongs to the seller).
///
/// # Errors
/// A fixing a period needs and does not have.
pub fn remaining(debt: &Debt, settle: Date, fixings: &dyn Fixings) -> Result<Vec<Cashflow>> {
    Ok(debt.cashflows(fixings)?.into_iter().filter(|c| c.pay > settle && !(c.kind == CashflowKind::Coupon && c.ex.is_some_and(|ex| ex <= settle))).collect())
}

fn per_100(debt: &Debt, settle: Date, value_per_unit: f64, fixings: &dyn Fixings) -> Result<Price> {
    let bal = debt.balance(settle);
    if bal <= 0.0 {
        return Err(Error::Invalid(format!("nothing is outstanding on {settle}")));
    }
    let accrued = debt.accrued(settle, fixings)? / bal * 100.0;
    let dirty = value_per_unit / bal * 100.0;
    Ok(Price { clean: dirty - accrued, accrued, dirty })
}

/// The price from a discount curve and a continuously compounded spread over it.
///
/// # Errors
/// Nothing outstanding at settlement, or a fixing a period needs.
pub fn price_from_curve(debt: &Debt, settle: Date, curve: &Curve, spread: f64, fixings: &dyn Fixings) -> Result<Price> {
    let ts = curve.time(settle);
    let base = curve.discount(ts) * (-spread * ts).exp();
    let v: f64 = remaining(debt, settle, fixings)?
        .iter()
        .map(|c| {
            let t = curve.time(c.pay);
            c.amount * curve.discount(t) * (-spread * t).exp()
        })
        .sum::<f64>()
        / base;
    per_100(debt, settle, v, fixings)
}

/// Coupon periods from `settle` to `date`: whole periods, plus the covered fraction of the ones
/// either end touches, by days (the street convention's exponent).
fn periods_between(debt: &Debt, settle: Date, date: Date) -> Result<f64> {
    let mut n = 0.0;
    for p in debt.periods()? {
        let (a, b) = (settle.max(p.start), date.min(p.end));
        if b > a {
            n += f64::from(a.days_to(b)) / f64::from(p.start.days_to(p.end));
        }
    }
    Ok(n)
}

/// The price from a yield.
///
/// # Errors
/// Nothing outstanding at settlement, or a fixing a period needs.
pub fn price_from_yield(debt: &Debt, settle: Date, y: f64, convention: YieldConvention, fixings: &dyn Fixings) -> Result<Price> {
    let f = f64::from(debt.schedule.per_year());
    let flows = remaining(debt, settle, fixings)?;
    let mut v = 0.0;
    for c in &flows {
        let df = match convention {
            // A yield counts to the scheduled date, not the business day it pays on.
            YieldConvention::Street if f > 0.0 => (1.0 + y / f).powf(-periods_between(debt, settle, c.date.max(settle))?),
            YieldConvention::Street | YieldConvention::Annual => (1.0 + y).powf(-f64::from(settle.days_to(c.date)) / 365.0),
            YieldConvention::Continuous => (-y * f64::from(settle.days_to(c.date)) / 365.0).exp(),
        };
        v += c.amount * df;
    }
    per_100(debt, settle, v, fixings)
}

/// The yield whose price is `clean`.
///
/// # Errors
/// A clean price no yield between −50% and 200% reaches.
pub fn yield_from_price(debt: &Debt, settle: Date, clean: f64, convention: YieldConvention, fixings: &dyn Fixings) -> Result<f64> {
    let target = clean;
    brent(|y| price_from_yield(debt, settle, y, convention, fixings).map(|p| p.clean - target), -0.5, 2.0, 1e-12, 200)
        .map_err(|_| Error::NoSolution(format!("no yield gives a clean price of {clean} on {settle}")))
}

/// The continuously compounded spread over `curve` whose price is `clean` (the Z-spread).
///
/// # Errors
/// A clean price no spread between −50% and 200% reaches.
pub fn z_spread(debt: &Debt, settle: Date, clean: f64, curve: &Curve, fixings: &dyn Fixings) -> Result<f64> {
    brent(|s| price_from_curve(debt, settle, curve, s, fixings).map(|p| p.clean - clean), -0.5, 2.0, 1e-12, 200)
        .map_err(|_| Error::NoSolution(format!("no spread gives a clean price of {clean} on {settle}")))
}

/// Macaulay duration (years), modified duration, and convexity, at a yield.
///
/// # Errors
/// As [`price_from_yield`].
pub fn durations(debt: &Debt, settle: Date, y: f64, convention: YieldConvention, fixings: &dyn Fixings) -> Result<(f64, f64, f64)> {
    let h = 1e-5;
    let p = price_from_yield(debt, settle, y, convention, fixings)?.dirty;
    let up = price_from_yield(debt, settle, y + h, convention, fixings)?.dirty;
    let down = price_from_yield(debt, settle, y - h, convention, fixings)?.dirty;
    let modified = (down - up) / (2.0 * h * p);
    let convexity = (up + down - 2.0 * p) / (h * h * p);
    let f = f64::from(debt.schedule.per_year());
    let macaulay = match convention {
        YieldConvention::Street if f > 0.0 => modified * (1.0 + y / f),
        YieldConvention::Street | YieldConvention::Annual => modified * (1.0 + y),
        YieldConvention::Continuous => modified,
    };
    Ok((macaulay, modified, convexity))
}

/// A sensitivity of the dirty price to `bump`, by central difference from a curve and spread.
///
/// # Errors
/// As [`price_from_curve`], or a key rate the curve does not have.
pub fn sensitivity(debt: &Debt, settle: Date, curve: &Curve, spread: f64, bump: Bump, fixings: &dyn Fixings) -> Result<Sensitivity> {
    let b = bump.size_bp;
    let (up, down) = match bump.kind {
        BumpKind::Parallel => {
            (price_from_curve(debt, settle, &curve.shifted(b), spread, fixings)?, price_from_curve(debt, settle, &curve.shifted(-b), spread, fixings)?)
        }
        BumpKind::KeyRate { point } => (
            price_from_curve(debt, settle, &curve.point_shifted(point, b)?, spread, fixings)?,
            price_from_curve(debt, settle, &curve.point_shifted(point, -b)?, spread, fixings)?,
        ),
        BumpKind::Spread => {
            (price_from_curve(debt, settle, curve, spread + b * 1e-4, fixings)?, price_from_curve(debt, settle, curve, spread - b * 1e-4, fixings)?)
        }
    };
    let what = match bump.kind {
        BumpKind::Parallel => "every zero rate of the discount curve".to_string(),
        BumpKind::KeyRate { point } => format!("the zero rate at {} years (a triangular key-rate bump)", curve.times[point]),
        BumpKind::Spread => "the continuously compounded spread over the curve".to_string(),
    };
    Ok(Sensitivity {
        bump,
        value: (down.dirty - up.dirty) / (2.0 * b),
        definition: format!(
            "change in dirty price per 100 of current face per 1bp fall in {what}: central difference of ±{b}bp, continuously compounded zeros on ACT/365F times, settlement {settle}, no recalibration"
        ),
    })
}

/// Brent's method on `[a, b]`.
fn brent(f: impl Fn(f64) -> Result<f64>, mut a: f64, mut b: f64, tol: f64, max: usize) -> Result<f64> {
    let (mut fa, mut fb) = (f(a)?, f(b)?);
    if fa * fb > 0.0 {
        return Err(Error::NoSolution("the bracket does not straddle a root".into()));
    }
    if fa.abs() < fb.abs() {
        std::mem::swap(&mut a, &mut b);
        std::mem::swap(&mut fa, &mut fb);
    }
    let (mut c, mut fc, mut d) = (a, fa, a);
    let mut mflag = true;
    for _ in 0..max {
        if fb == 0.0 || (b - a).abs() < tol {
            return Ok(b);
        }
        let mut s = if fa != fc && fb != fc {
            a * fb * fc / ((fa - fb) * (fa - fc)) + b * fa * fc / ((fb - fa) * (fb - fc)) + c * fa * fb / ((fc - fa) * (fc - fb))
        } else {
            b - fb * (b - a) / (fb - fa)
        };
        let lo = (3.0 * a + b) / 4.0;
        let between = if lo < b { s > lo && s < b } else { s > b && s < lo };
        if !between || (mflag && (s - b).abs() >= (b - c).abs() / 2.0) || (!mflag && (s - b).abs() >= (c - d).abs() / 2.0) {
            s = (a + b) / 2.0;
            mflag = true;
        } else {
            mflag = false;
        }
        let fs = f(s)?;
        d = c;
        c = b;
        fc = fb;
        if fa * fs < 0.0 {
            b = s;
            fb = fs;
        } else {
            a = s;
            fa = fs;
        }
        if fa.abs() < fb.abs() {
            std::mem::swap(&mut a, &mut b);
            std::mem::swap(&mut fa, &mut fb);
        }
    }
    Err(Error::NoSolution("no convergence".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coupon::NoFixings;
    use crate::daycount::DayCount;

    fn d(s: &str) -> Date {
        s.parse().unwrap()
    }

    fn bond(rate: f64) -> Debt {
        Debt::fixed("USD", d("2026-01-15"), d("2036-01-15"), rate, 2, DayCount::ActActIcma).unwrap()
    }

    #[test]
    fn a_bond_on_a_coupon_date_is_at_par_when_its_yield_is_its_coupon() {
        let b = bond(0.05);
        let p = price_from_yield(&b, d("2026-01-15"), 0.05, YieldConvention::Street, &NoFixings).unwrap();
        assert!((p.clean - 100.0).abs() < 1e-9 && p.accrued.abs() < 1e-12, "{p:?}");
        // The annuity formula: a 5% bond at 6% for ten years, semiannual.
        let (c, y, n) = (2.5, 0.03, 20);
        let want = c * (1.0 - (1.0f64 + y).powi(-n)) / y + 100.0 * (1.0f64 + y).powi(-n);
        let q = price_from_yield(&b, d("2026-01-15"), 0.06, YieldConvention::Street, &NoFixings).unwrap();
        assert!((q.clean - want).abs() < 1e-9, "{} vs {want}", q.clean);
    }

    #[test]
    fn yield_and_price_invert_and_between_coupons_the_accrued_is_split_out() {
        let b = bond(0.0425);
        let settle = d("2027-05-20");
        let p = price_from_yield(&b, settle, 0.051, YieldConvention::Street, &NoFixings).unwrap();
        let y = yield_from_price(&b, settle, p.clean, YieldConvention::Street, &NoFixings).unwrap();
        assert!((y - 0.051).abs() < 1e-10, "{y}");
        // 15 Jan to 20 May 2027 is 125 days of a 181-day period.
        assert!((p.accrued - 4.25 / 2.0 * 125.0 / 181.0).abs() < 1e-12);
        assert!((p.dirty - p.clean - p.accrued).abs() < 1e-12);
    }

    #[test]
    fn a_curve_prices_and_the_z_spread_recovers_its_spread() {
        let b = bond(0.04);
        let settle = d("2027-03-01");
        let curve = Curve::new(settle, vec![0.5, 2.0, 5.0, 10.0], vec![0.030, 0.034, 0.037, 0.040], crate::Interpolation::LinearZero).unwrap();
        let p = price_from_curve(&b, settle, &curve, 0.0125, &NoFixings).unwrap();
        let s = z_spread(&b, settle, p.clean, &curve, &NoFixings).unwrap();
        assert!((s - 0.0125).abs() < 1e-10, "{s}");
        // On a flat curve at the bond's continuously compounded equivalent of its yield, the two agree.
        let flat = Curve::flat(settle, 2.0 * (1.0f64 + 0.045 / 2.0).ln());
        let from_curve = price_from_curve(&b, settle, &flat, 0.0, &NoFixings).unwrap().dirty;
        let from_yield = price_from_yield(&b, settle, 0.045, YieldConvention::Street, &NoFixings).unwrap().dirty;
        assert!((from_curve - from_yield).abs() < 0.05, "{from_curve} vs {from_yield}: only the street convention's day fractions differ");
    }

    #[test]
    fn sensitivities_match_finite_differences_and_key_rates_add_up() {
        let b = bond(0.04);
        let settle = d("2027-03-01");
        let curve = Curve::new(settle, vec![1.0, 3.0, 5.0, 10.0], vec![0.03, 0.033, 0.036, 0.04], crate::Interpolation::LinearZero).unwrap();
        let dv01 = sensitivity(&b, settle, &curve, 0.0, Bump { kind: BumpKind::Parallel, size_bp: 1.0 }, &NoFixings).unwrap();
        assert!(dv01.value > 0.0 && dv01.definition.contains("no recalibration"));
        let krs: f64 =
            (0..4).map(|i| sensitivity(&b, settle, &curve, 0.0, Bump { kind: BumpKind::KeyRate { point: i }, size_bp: 1.0 }, &NoFixings).unwrap().value).sum();
        assert!((krs - dv01.value).abs() < 1e-3 * dv01.value, "key rates {krs} vs parallel {}", dv01.value);
        let cs01 = sensitivity(&b, settle, &curve, 0.0, Bump { kind: BumpKind::Spread, size_bp: 1.0 }, &NoFixings).unwrap();
        assert!((cs01.value - dv01.value).abs() < 1e-9, "a spread and a parallel shift are the same move here");
        let (mac, modi, conv) = durations(&b, settle, 0.04, YieldConvention::Street, &NoFixings).unwrap();
        assert!(mac > modi && modi > 7.0 && modi < 9.0 && conv > 0.0, "{mac} {modi} {conv}");
    }

    #[test]
    fn a_coupon_gone_ex_belongs_to_the_seller() {
        let mut b = bond(0.04);
        b.schedule.ex_days = 7;
        let before = remaining(&b, d("2026-07-01"), &NoFixings).unwrap();
        let after = remaining(&b, d("2026-07-08"), &NoFixings).unwrap();
        assert_eq!(before.len(), after.len() + 1, "the 15 July coupon went ex on 6 July");
        let flat = Curve::flat(d("2026-07-08"), 0.04);
        let p = price_from_curve(&b, d("2026-07-08"), &flat, 0.0, &NoFixings).unwrap();
        assert!(p.accrued < 0.0, "negative accrued in the ex period: {p:?}");
    }
}
