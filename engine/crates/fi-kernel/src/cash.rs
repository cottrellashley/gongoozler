//! Exact cash: a fixed-point decimal, as Nautilus's high-precision `Money`.
//!
//! Cash is an `i128` with 16 decimal places. A float becomes cash once, as its exact binary value
//! rounded half to even at 16 places, so an amount a contract states (0.011875, 593,750) is that
//! decimal exactly. After that, sums are exact, so the ledger's cash never drifts with the order of
//! additions, and a quantity times an amount per unit is multiplied in decimal ([`Cash::product`]),
//! never in binary (review H4).
//!
//! Contracts pay whole minor units of their currency (spec I1: "cash to the smallest unit of the
//! currency"). How an amount becomes one is the contract's own term, a [`Rounding`]: a deal's
//! payments and cures, an agreement's lendable capacity, a bond's coupon per unit.

use std::ops::{Add, AddAssign, Mul, Neg, Sub, SubAssign};

/// Cash with 16 decimal places.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Cash(pub i128);

impl Cash {
    /// One unit of currency, in raw units.
    pub const SCALE: i128 = 10_000_000_000_000_000;
    /// Zero.
    pub const ZERO: Cash = Cash(0);

    /// From a float: the decimal it states. A float holds every decimal of up to 15 significant
    /// digits exactly (0.011875, 1234.5678, 593,750), so the float is read as its value rounded to
    /// 15 significant digits, and to at most 16 decimal places, half to even. The rounding is of
    /// the exact binary value, in integers.
    ///
    /// # Panics
    /// If `x` is not finite (cash is never NaN or infinite, principle 7), or beyond about 1.7e22.
    #[must_use]
    pub fn from_f64(x: f64) -> Self {
        assert!(x.is_finite(), "cash must be finite, got {x}");
        if x == 0.0 {
            return Cash::ZERO;
        }
        let bits = x.to_bits();
        let neg = bits >> 63 == 1;
        let exp = ((bits >> 52) & 0x7ff) as i32;
        let frac = u128::from(bits & ((1 << 52) - 1));
        // |x| = m × 2^e exactly.
        let (m, e) = if exp == 0 { (frac, -1074) } else { (frac | (1 << 52), exp - 1075) };
        // Decimal places for 15 significant digits, at most 16: floor(log10 |x|) from the binary
        // exponent (78913 / 2^18 is log10 2), corrected against exact powers of ten.
        let a = x.abs();
        let mut d = ((exp.max(1) - 1023) * 78913) >> 18;
        if pow10(d + 1) <= a {
            d += 1;
        }
        let places = (14 - d).min(16);
        let raw = if places >= 0 {
            // round(m × 2^e × 10^places), then scaled to 16 places.
            let n = round_shift(m * POW10_INT[places as usize], e);
            n.checked_mul(POW10_INT[(16 - places) as usize])
        } else {
            // |x| ≥ 1e15: an integer to its 15 significant digits.
            let v = (e >= 0).then(|| m.checked_shl(e as u32)).flatten().unwrap_or_else(|| round_shift(m, e));
            let unit = 10u128.pow((-places) as u32);
            let (q, r) = (v / unit, v % unit);
            let q = if r > unit / 2 || (r == unit / 2 && q & 1 == 1) { q + 1 } else { q };
            q.checked_mul(unit).and_then(|n| n.checked_mul(Cash::SCALE as u128))
        };
        let raw = raw.and_then(|r| i128::try_from(r).ok()).expect("cash beyond 1.7e22 units");
        Cash(if neg { -raw } else { raw })
    }

    /// `quantity × per_unit`, in decimal: each as the decimal it denotes ([`Cash::from_f64`]),
    /// multiplied exactly (`Cash * Cash`). What a book's flow is: its quantity times what one unit
    /// pays.
    #[must_use]
    pub fn product(quantity: f64, per_unit: f64) -> Cash {
        Cash::from_f64(quantity) * Cash::from_f64(per_unit)
    }

    /// As a float.
    #[must_use]
    pub fn to_f64(self) -> f64 {
        let whole = self.0 / Self::SCALE;
        let frac = self.0 % Self::SCALE;
        whole as f64 + frac as f64 / 1e16
    }
}

/// 10^k as integers, for k in 0..=16.
const POW10_INT: [u128; 17] = {
    let mut t = [1u128; 17];
    let mut k = 1;
    while k < 17 {
        t[k] = t[k - 1] * 10;
        k += 1;
    }
    t
};

/// 10^k as the nearest float (exact for k in 0..=22).
fn pow10(k: i32) -> f64 {
    const POS: [f64; 23] = [1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16, 1e17, 1e18, 1e19, 1e20, 1e21, 1e22];
    const NEG: [f64; 23] =
        [1e0, 1e-1, 1e-2, 1e-3, 1e-4, 1e-5, 1e-6, 1e-7, 1e-8, 1e-9, 1e-10, 1e-11, 1e-12, 1e-13, 1e-14, 1e-15, 1e-16, 1e-17, 1e-18, 1e-19, 1e-20, 1e-21, 1e-22];
    match k {
        0..=22 => POS[k as usize],
        -22..=-1 => NEG[(-k) as usize],
        _ => 10f64.powi(k),
    }
}

/// `round(n × 2^e)`, half to even, for `n` below 2^120.
fn round_shift(n: u128, e: i32) -> u128 {
    if e >= 0 {
        return n.checked_shl(e as u32).filter(|v| v >> (e as u32) == n).expect("cash beyond 1.7e22 units");
    }
    let k = (-e) as u32;
    if k >= 127 {
        return 0;
    }
    let (q, r, half) = (n >> k, n & ((1 << k) - 1), 1u128 << (k - 1));
    if r > half || (r == half && q & 1 == 1) { q + 1 } else { q }
}

impl Add for Cash {
    type Output = Cash;
    fn add(self, rhs: Cash) -> Cash {
        Cash(self.0 + rhs.0)
    }
}

/// The product, exact: two decimals multiplied, rounded half to even at 16 places.
///
/// # Panics
/// If the product is beyond about 1.7e22 units.
impl Mul for Cash {
    type Output = Cash;

    fn mul(self, rhs: Cash) -> Cash {
        const S: u128 = Cash::SCALE as u128;
        let neg = (self.0 < 0) != (rhs.0 < 0);
        let (a, b) = (self.0.unsigned_abs(), rhs.0.unsigned_abs());
        // a·b/S = ah·bh·S + ah·bl + al·bh + al·bl/S, with a = ah·S + al and b = bh·S + bl: only
        // the last part has a fraction, and al·bl < 1e32 fits.
        let (ah, al, bh, bl) = (a / S, a % S, b / S, b % S);
        let product = || -> Option<u128> {
            let low = al * bl;
            let (q, r) = (low / S, low % S);
            let mut x = ah.checked_mul(bh)?.checked_mul(S)?.checked_add(ah.checked_mul(bl)?)?.checked_add(al.checked_mul(bh)?)?.checked_add(q)?;
            if r > S / 2 || (r == S / 2 && x & 1 == 1) {
                x = x.checked_add(1)?;
            }
            i128::try_from(x).ok().map(|_| x)
        };
        let x = product().expect("a product of cash beyond 1.7e22 units") as i128;
        Cash(if neg { -x } else { x })
    }
}

impl Sub for Cash {
    type Output = Cash;
    fn sub(self, rhs: Cash) -> Cash {
        Cash(self.0 - rhs.0)
    }
}

impl AddAssign for Cash {
    fn add_assign(&mut self, rhs: Cash) {
        self.0 += rhs.0;
    }
}

impl SubAssign for Cash {
    fn sub_assign(&mut self, rhs: Cash) {
        self.0 -= rhs.0;
    }
}

impl Neg for Cash {
    type Output = Cash;
    fn neg(self) -> Cash {
        Cash(-self.0)
    }
}

/// Which way an amount moves to a whole number of units.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum RoundingMode {
    /// Down, toward minus infinity (floor).
    Down,
    /// Up, toward plus infinity (ceiling).
    Up,
    /// To the nearest; a half away from zero.
    #[default]
    HalfUp,
    /// To the nearest; a half to the even unit.
    HalfEven,
}

/// A contract's rounding term: amounts become whole multiples of `unit` (a currency's minor unit,
/// 0.01 for cents) the way `mode` says. Floating-point arithmetic leaves noise around exact amounts
/// (25,000,000 × 0.94 is 23,499,999.999999998), and it grows with the amount. So an amount
/// within 1e-14 (relative, some sixty ulps) of a whole number of units, or of an exact half,
/// counts as that amount: noise never moves it by a unit, while a real fraction of a cent still
/// does on any amount up to tens of billions.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Rounding {
    /// The unit, in currency (0.01: cents).
    pub unit: f64,
    /// The rule.
    pub mode: RoundingMode,
}

impl Rounding {
    /// Whole cents, by `mode`.
    #[must_use]
    pub const fn cents(mode: RoundingMode) -> Self {
        Self { unit: 0.01, mode }
    }

    /// `x` in whole units by the term's own mode. A non-finite amount is returned as it is.
    #[must_use]
    pub fn round(&self, x: f64) -> f64 {
        self.by(x, self.mode)
    }

    /// `x` in whole units, rounded down.
    #[must_use]
    pub fn down(&self, x: f64) -> f64 {
        self.by(x, RoundingMode::Down)
    }

    /// `x` in whole units, rounded up: the smallest whole amount not below it.
    #[must_use]
    pub fn up(&self, x: f64) -> f64 {
        self.by(x, RoundingMode::Up)
    }

    fn by(&self, x: f64, mode: RoundingMode) -> f64 {
        if !x.is_finite() || self.unit.is_nan() || self.unit <= 0.0 {
            return x;
        }
        // Units per currency unit, exactly where it is whole (100 for cents): multiplying by it
        // is exact where dividing by 0.01 is not.
        let per = 1.0 / self.unit;
        let scale = (per.round() - per).abs() < 1e-9 * per.max(1.0);
        let q = if scale { x * per.round() } else { x / self.unit };
        let tol = (q.abs() * 1e-14).max(1e-9);
        let floor = q.floor();
        let q = if (q - q.round()).abs() <= tol {
            q.round()
        } else if (q - floor - 0.5).abs() <= tol {
            floor + 0.5
        } else {
            q
        };
        let k = match mode {
            RoundingMode::Down => q.floor(),
            RoundingMode::Up => q.ceil(),
            RoundingMode::HalfUp => q.round(),
            RoundingMode::HalfEven => q.round_ties_even(),
        };
        if scale { k / per.round() } else { k * self.unit }
    }
}

#[cfg(test)]
mod tests {
    use super::{Cash, Rounding, RoundingMode};

    #[test]
    fn rounding_is_exact_in_minor_units() {
        let c = Rounding::cents(RoundingMode::Down);
        // 1,000,000 × 0.95 carries float noise below the unit; it is still 950,000.00.
        assert_eq!(c.down(1_000_000.0 * (1.0 - 0.05)), 950_000.0);
        assert_eq!(c.down(102.0 / 1.03 * 100.0 / 100.0), 99.02);
        assert_eq!(c.up(100.0 - 102.0 / 1.03), 0.98);
        assert_eq!(c.up(0.97), 0.97);
        assert_eq!(c.up(1e-14), 0.0, "float noise is not a cent");
        assert_eq!(c.down(-0.001), -0.01);
        let h = Rounding::cents(RoundingMode::HalfUp);
        assert_eq!(h.round(0.125), 0.13);
        assert_eq!(h.round(-0.125), -0.13);
        assert_eq!(Rounding::cents(RoundingMode::HalfEven).round(0.125), 0.12);
        assert_eq!(Rounding::cents(RoundingMode::HalfEven).round(0.135), 0.14);
        assert_eq!(Rounding { unit: 1.0, mode: RoundingMode::HalfUp }.round(2.5), 3.0);
        assert!(h.round(f64::NAN).is_nan());
        // Noise grows with the amount; it still never moves an exact amount by a cent.
        assert_eq!(c.down(25_000_000.0 * 0.94), 23_500_000.0);
        assert_eq!(c.down(25_000_000.0 * (1.0 - 0.0433)), 23_917_500.0);
        assert_eq!(c.down(1e9 + 0.019), 1e9 + 0.01, "a real fraction of a cent still rounds");
        // A decimal half is a half, though binary cannot hold 0.145 exactly.
        assert_eq!(h.round(0.145), 0.15);
        assert_eq!(Rounding::cents(RoundingMode::HalfEven).round(0.145), 0.14);
        assert_eq!(Rounding { unit: 0.25, mode: RoundingMode::Up }.round(1.1), 1.25);
    }

    #[test]
    fn sums_are_exact_in_any_order() {
        let xs = [0.1, 0.2, 0.3, 1e9 + 0.01, -0.7, 12_345.678_9];
        let fwd = xs.iter().fold(Cash::ZERO, |a, x| a + Cash::from_f64(*x));
        let rev = xs.iter().rev().fold(Cash::ZERO, |a, x| a + Cash::from_f64(*x));
        assert_eq!(fwd, rev);
        assert!((fwd.to_f64() - xs.iter().sum::<f64>()).abs() < 1e-6);
    }

    #[test]
    fn floats_become_the_decimals_they_state_and_products_are_decimal() {
        // x·1e16 in binary is not exact past 2^53: these were off in their last digits.
        assert_eq!(Cash::from_f64(593_750.0), Cash(593_750 * Cash::SCALE));
        assert_eq!(Cash::from_f64(50_000_000.0), Cash(50_000_000 * Cash::SCALE));
        assert_eq!(Cash::from_f64(0.011875), Cash(118_750_000_000_000));
        assert_eq!(Cash::from_f64(-1234.5678), Cash(-12_345_678_000_000_000_000));
        assert_eq!(Cash::from_f64(0.1 + 0.2), Cash::from_f64(0.3), "binary noise is not a decimal");
        assert_eq!(Cash::from_f64(1e-17), Cash::ZERO);
        assert_eq!(Cash::from_f64(7e-17), Cash(1));
        assert_eq!(Cash::from_f64(f64::MIN_POSITIVE), Cash::ZERO);
        assert_eq!(Cash::from_f64(1e15), Cash(1_000_000_000_000_000 * Cash::SCALE));
        assert_eq!(Cash::from_f64(123_456_789_012_345_680.0), Cash(123_456_789_012_346_000 * Cash::SCALE), "15 significant digits");
        assert_eq!(Cash::from_f64(100.0).to_f64(), 100.0);
        // 2.375% semi-annual on 50,000,000 face: exact in decimal, not in binary.
        assert_eq!(Cash::product(50_000_000.0, 0.011875), Cash(593_750 * Cash::SCALE));
        assert_eq!(Cash::product(-3.0, 0.1), Cash::from_f64(-0.3));
        assert_eq!(Cash::product(1e10, 1.0), Cash(10_000_000_000 * Cash::SCALE), "large quantities do not overflow");
        assert_eq!(Cash(1) * Cash(Cash::SCALE / 2), Cash::ZERO, "half of the smallest unit rounds to even");
        assert_eq!(Cash(3) * Cash(Cash::SCALE / 2), Cash(2));
        let x = 98_765_432.123_456_78;
        assert!((Cash::product(x, 0.0151666667).to_f64() - x * 0.0151666667).abs() < 1e-6);
    }

    #[test]
    #[should_panic(expected = "finite")]
    fn refuses_nan() {
        let _ = Cash::from_f64(f64::NAN);
    }
}
