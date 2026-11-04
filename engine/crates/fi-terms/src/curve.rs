//! Discount curves from observed points, with a declared interpolation.

use crate::coupon::Fixings;
use crate::date::Date;
use crate::daycount::DayCount;
use crate::{Error, Result};

/// How a curve is read between its points.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum Interpolation {
    /// Zero rates linear in time; flat beyond the ends.
    #[default]
    LinearZero,
    /// Log discount factors linear in time (forward rates flat between points); the last forward
    /// carries on beyond the end.
    LogLinearDiscount,
}

/// A discount curve: continuously compounded zero rates at times in years (ACT/365F) from its
/// reference date.
#[derive(Clone, Debug, PartialEq)]
pub struct Curve {
    /// The date its times are measured from.
    pub reference: Date,
    /// Times in years, increasing and positive.
    pub times: Vec<f64>,
    /// Zero rates, continuously compounded.
    pub zeros: Vec<f64>,
    /// How it is read between points.
    pub interpolation: Interpolation,
}

impl Curve {
    /// A curve through `zeros` (continuously compounded) at `times`.
    ///
    /// # Errors
    /// No points, lengths that differ, times that do not increase from above zero, or values that
    /// are not finite.
    pub fn new(reference: Date, times: Vec<f64>, zeros: Vec<f64>, interpolation: Interpolation) -> Result<Self> {
        if times.is_empty() || times.len() != zeros.len() {
            return Err(Error::Invalid(format!("a curve needs points: {} times, {} rates", times.len(), zeros.len())));
        }
        if times[0] <= 0.0 || times.windows(2).any(|w| w[1] <= w[0]) || times.iter().chain(&zeros).any(|x| !x.is_finite()) {
            return Err(Error::Invalid("a curve's times must increase from above zero, and every value be finite".into()));
        }
        Ok(Self { reference, times, zeros, interpolation })
    }

    /// A curve through discount factors.
    ///
    /// # Errors
    /// As [`Curve::new`], or a discount factor that is not positive.
    pub fn from_discount_factors(reference: Date, times: Vec<f64>, dfs: &[f64], interpolation: Interpolation) -> Result<Self> {
        if dfs.iter().any(|d| d.is_nan() || *d <= 0.0) {
            return Err(Error::Invalid("a discount factor that is not positive".into()));
        }
        let zeros = times.iter().zip(dfs).map(|(t, d)| -d.ln() / t).collect();
        Self::new(reference, times, zeros, interpolation)
    }

    /// One rate everywhere.
    #[must_use]
    pub fn flat(reference: Date, rate: f64) -> Self {
        Self { reference, times: vec![1.0], zeros: vec![rate], interpolation: Interpolation::LinearZero }
    }

    /// Years from the reference date to `d` (ACT/365F).
    #[must_use]
    pub fn time(&self, d: Date) -> f64 {
        f64::from(self.reference.days_to(d)) / 365.0
    }

    /// The zero rate at `t` years.
    #[must_use]
    pub fn zero(&self, t: f64) -> f64 {
        if t <= 0.0 {
            return self.zeros[0];
        }
        match self.interpolation {
            Interpolation::LinearZero => {
                let n = self.times.len();
                if t <= self.times[0] {
                    return self.zeros[0];
                }
                if t >= self.times[n - 1] {
                    return self.zeros[n - 1];
                }
                let i = self.times.partition_point(|x| *x <= t);
                let (t0, t1) = (self.times[i - 1], self.times[i]);
                self.zeros[i - 1] + (t - t0) / (t1 - t0) * (self.zeros[i] - self.zeros[i - 1])
            }
            Interpolation::LogLinearDiscount => -self.log_discount(t) / t,
        }
    }

    fn log_discount(&self, t: f64) -> f64 {
        let n = self.times.len();
        let ld = |i: usize| -self.zeros[i] * self.times[i];
        if t <= self.times[0] {
            return -self.zeros[0] * t;
        }
        if t >= self.times[n - 1] {
            let f = if n > 1 { (ld(n - 2) - ld(n - 1)) / (self.times[n - 1] - self.times[n - 2]) } else { self.zeros[0] };
            return ld(n - 1) - f * (t - self.times[n - 1]);
        }
        let i = self.times.partition_point(|x| *x <= t);
        let (t0, t1) = (self.times[i - 1], self.times[i]);
        ld(i - 1) + (t - t0) / (t1 - t0) * (ld(i) - ld(i - 1))
    }

    /// The discount factor at `t` years.
    #[must_use]
    pub fn discount(&self, t: f64) -> f64 {
        match self.interpolation {
            Interpolation::LinearZero => (-self.zero(t) * t).exp(),
            Interpolation::LogLinearDiscount => self.log_discount(t).exp(),
        }
    }

    /// The discount factor to `d`.
    #[must_use]
    pub fn discount_to(&self, d: Date) -> f64 {
        self.discount(self.time(d))
    }

    /// The simple forward rate from `d1` to `d2`, in `dc`.
    #[must_use]
    pub fn forward(&self, d1: Date, d2: Date, dc: DayCount) -> f64 {
        let tau = dc.fraction(d1, d2, None, None).unwrap_or_else(|_| f64::from(d1.days_to(d2)) / 365.0);
        if tau <= 0.0 {
            return self.zero(self.time(d1));
        }
        (self.discount_to(d1) / self.discount_to(d2) - 1.0) / tau
    }

    /// Every zero rate moved by `bp` basis points.
    #[must_use]
    pub fn shifted(&self, bp: f64) -> Self {
        let mut c = self.clone();
        for z in &mut c.zeros {
            *z += bp * 1e-4;
        }
        c
    }

    /// One point's zero rate moved by `bp`: with linear zero interpolation, a triangular bump
    /// around that point (a key-rate shift).
    ///
    /// # Errors
    /// A point the curve does not have.
    pub fn point_shifted(&self, point: usize, bp: f64) -> Result<Self> {
        let mut c = self.clone();
        let z = c.zeros.get_mut(point).ok_or_else(|| Error::Invalid(format!("the curve has {} points, not {}", self.times.len(), point + 1)))?;
        *z += bp * 1e-4;
        Ok(c)
    }
}

/// Fixings that are observed where known and projected from a curve where not: forwards over
/// `tenor_months` (0 for an overnight rate) in `day_count`. For pricing a floater in a
/// simulation or scenario, never for what a run observed.
pub struct Projected<'a> {
    /// The fixings known.
    pub observed: &'a dyn Fixings,
    /// The index projected.
    pub index: String,
    /// The projection curve.
    pub curve: &'a Curve,
    /// The rate's term (0: overnight).
    pub tenor_months: u32,
    /// The rate's day count.
    pub day_count: DayCount,
}

impl<'a> Projected<'a> {
    /// The projection a floating coupon of `debt` needs to be valued off `curve`: forwards over
    /// the rate's own term (the schedule's period for a rate fixed in advance, overnight for one
    /// compounded or averaged in arrears) in the debt's day count, observed fixings first. `None`
    /// for any other coupon (an inflation index is a level, not a rate a curve projects).
    #[must_use]
    pub fn for_debt(debt: &crate::Debt, observed: &'a dyn Fixings, curve: &'a Curve) -> Option<Self> {
        let crate::Coupon::Floating { index, method, .. } = &debt.coupon else { return None };
        let tenor_months = match method {
            crate::Method::InAdvance { .. } => debt.schedule.months,
            _ => 0,
        };
        Some(Self { observed, index: index.clone(), curve, tenor_months, day_count: debt.day_count })
    }
}

impl Fixings for Projected<'_> {
    fn fixing(&self, index: &str, date: Date) -> Option<f64> {
        if let Some(v) = self.observed.fixing(index, date) {
            return Some(v);
        }
        if index != self.index {
            return None;
        }
        let end = if self.tenor_months == 0 { date.plus_days(1) } else { date.plus_months(self.tenor_months as i32, false) };
        Some(self.curve.forward(date.max(self.curve.reference), end.max(self.curve.reference.plus_days(1)), self.day_count))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Date {
        s.parse().unwrap()
    }

    #[test]
    fn both_interpolations_pass_through_their_points() {
        for interp in [Interpolation::LinearZero, Interpolation::LogLinearDiscount] {
            let c = Curve::new(d("2026-01-02"), vec![0.5, 2.0, 10.0], vec![0.03, 0.035, 0.04], interp).unwrap();
            for (t, z) in [(0.5, 0.03), (2.0, 0.035), (10.0, 0.04)] {
                assert!((c.zero(t) - z).abs() < 1e-14 && (c.discount(t) - (-z * t).exp()).abs() < 1e-14, "{interp:?} at {t}");
            }
        }
        let lin = Curve::new(d("2026-01-02"), vec![1.0, 3.0], vec![0.02, 0.04], Interpolation::LinearZero).unwrap();
        assert!((lin.zero(2.0) - 0.03).abs() < 1e-15 && (lin.zero(9.0) - 0.04).abs() < 1e-15 && (lin.zero(0.1) - 0.02).abs() < 1e-15);
        // Log-linear discounting: the forward between points is flat.
        let ll = Curve::new(d("2026-01-02"), vec![1.0, 3.0], vec![0.02, 0.04], Interpolation::LogLinearDiscount).unwrap();
        let fwd: f64 = (0.04 * 3.0 - 0.02 * 1.0) / 2.0;
        assert!((ll.discount(2.0) - (-(0.02 + fwd)).exp()).abs() < 1e-14);
    }

    #[test]
    fn bad_points_are_refused_and_shifts_move_what_they_say() {
        assert!(Curve::new(d("2026-01-02"), vec![1.0, 1.0], vec![0.01, 0.02], Interpolation::LinearZero).is_err());
        assert!(Curve::from_discount_factors(d("2026-01-02"), vec![1.0], &[0.0], Interpolation::LinearZero).is_err());
        let c = Curve::new(d("2026-01-02"), vec![1.0, 5.0], vec![0.02, 0.03], Interpolation::LinearZero).unwrap();
        assert!((c.shifted(10.0).zero(3.0) - (c.zero(3.0) + 0.001)).abs() < 1e-15);
        let k = c.point_shifted(1, 10.0).unwrap();
        assert!((k.zero(5.0) - 0.031).abs() < 1e-15 && (k.zero(1.0) - 0.02).abs() < 1e-15 && (k.zero(3.0) - (c.zero(3.0) + 0.0005)).abs() < 1e-15);
        assert!(c.point_shifted(2, 1.0).is_err());
        let f = Curve::flat(d("2026-01-02"), 0.05).forward(d("2026-04-02"), d("2026-07-02"), DayCount::Act365F);
        assert!((f - ((0.05f64 * 91.0 / 365.0).exp() - 1.0) / (91.0 / 365.0)).abs() < 1e-14);
    }
}
