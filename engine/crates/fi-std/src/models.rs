//! Example models on the kernel's model interface. Each is a surface-level definition built from
//! the core objects, not part of the core.
//!
//! * [`ZScore`]: cross-sectional z-score, optionally within groups (sectors through a category).
//! * [`NelsonSiegel`]: a fair-value rates model; a cross-sectional curve fit each date.
//! * [`EwmaVol`]: exponentially weighted volatility; a time series per entity.
//! * [`Merton`]: structural credit; distance to default, default probability and spread per issuer.

use std::collections::HashMap;
use std::f64::consts::SQRT_2;

use fi_kernel::{CrossSectionModel, EntityId, Input, ModelSpec, Result, Shape, TimeSeriesModel, Ts, hash::Hash64};

/// The standard normal distribution function.
#[must_use]
pub fn norm_cdf(x: f64) -> f64 {
    0.5 * libm::erfc(-x / SQRT_2)
}

/// Cross-sectional z-score, within groups when a group input is given.
#[derive(Clone, Debug)]
pub struct ZScore {
    /// Model name (its output series).
    pub name: String,
    /// The value to standardise.
    pub input: Input,
    /// Optional group code per entity (a sector), for example through a relationship.
    pub group: Option<Input>,
    /// Degrees of freedom removed from the variance (0: population, as numpy's `nanstd`).
    pub ddof: f64,
    /// Fewest values a group needs.
    pub min_count: usize,
}

impl CrossSectionModel for ZScore {
    fn spec(&self) -> ModelSpec {
        let mut h = Hash64::new();
        h.f64(self.ddof);
        h.u64(self.min_count as u64);
        let mut s = ModelSpec::new(&self.name, Shape::CrossSection).input(self.input.clone()).output("z").params(h.finish());
        if let Some(g) = &self.group {
            s = s.input(g.clone());
        }
        // Read through a relationship, the universe is the relationship's entities (the bonds).
        if let Some((rel, _)) = &self.input.via {
            s = s.universe(rel);
        }
        s
    }

    fn compute(&self, _t: Ts, universe: &[EntityId], x: &[Vec<f64>]) -> Result<Vec<Vec<f64>>> {
        let n = universe.len();
        let mut groups: HashMap<u64, Vec<usize>> = HashMap::new();
        for j in 0..n {
            if !x[0][j].is_finite() {
                continue;
            }
            let g = match x.get(1) {
                Some(codes) if codes[j].is_finite() => codes[j].to_bits(),
                Some(_) => continue,
                None => 0,
            };
            groups.entry(g).or_default().push(j);
        }
        let mut z = vec![f64::NAN; n];
        for idx in groups.values() {
            let m = idx.len();
            if m < self.min_count.max(1) || (m as f64) <= self.ddof {
                continue;
            }
            let mean = idx.iter().map(|&j| x[0][j]).sum::<f64>() / m as f64;
            let var = idx.iter().map(|&j| (x[0][j] - mean).powi(2)).sum::<f64>() / (m as f64 - self.ddof);
            let sd = var.sqrt();
            if sd > 0.0 {
                for &j in idx {
                    z[j] = (x[0][j] - mean) / sd;
                }
            }
        }
        Ok(vec![z])
    }
}

/// A Nelson–Siegel fair-value curve, fitted each date across a universe of bonds.
///
/// With λ fixed the fit is linear least squares on [1, f₁(τ), f₂(τ)], where
/// f₁ = (1 − e^(−τ/λ)) / (τ/λ) and f₂ = f₁ − e^(−τ/λ). Outputs the fitted yield and the
/// residual (market − fitted): rich (negative) or cheap (positive).
#[derive(Clone, Debug)]
pub struct NelsonSiegel {
    /// Model name.
    pub name: String,
    /// Yields (fractions).
    pub yields: Input,
    /// Maturity, as days since 1970-01-01 (read as of the date).
    pub maturity: Input,
    /// Decay, in years.
    pub lambda: f64,
    /// Fewest bonds for a fit.
    pub min_points: usize,
}

/// The Nelson–Siegel loadings at maturity `tau` years.
#[must_use]
pub fn ns_basis(tau: f64, lambda: f64) -> [f64; 3] {
    let x = tau / lambda;
    let e = (-x).exp();
    let f1 = if x.abs() < 1e-10 { 1.0 } else { (1.0 - e) / x };
    [1.0, f1, f1 - e]
}

/// Least squares β for rows (basis, y): solves the 3×3 normal equations with partial pivoting.
#[must_use]
pub fn least_squares3(rows: &[([f64; 3], f64)]) -> Option<[f64; 3]> {
    let mut a = [[0.0f64; 4]; 3];
    for (b, y) in rows {
        for i in 0..3 {
            for j in 0..3 {
                a[i][j] += b[i] * b[j];
            }
            a[i][3] += b[i] * y;
        }
    }
    for c in 0..3 {
        let p = (c..3).max_by(|&i, &j| a[i][c].abs().total_cmp(&a[j][c].abs()))?;
        if a[p][c].abs() < 1e-14 {
            return None;
        }
        a.swap(c, p);
        let pivot = a[c];
        for (r, row) in a.iter_mut().enumerate() {
            if r != c {
                let f = row[c] / pivot[c];
                for (x, p) in row[c..].iter_mut().zip(&pivot[c..]) {
                    *x -= f * p;
                }
            }
        }
    }
    Some([a[0][3] / a[0][0], a[1][3] / a[1][1], a[2][3] / a[2][2]])
}

impl CrossSectionModel for NelsonSiegel {
    fn spec(&self) -> ModelSpec {
        let mut h = Hash64::new();
        h.f64(self.lambda);
        h.u64(self.min_points as u64);
        ModelSpec::new(&self.name, Shape::CrossSection)
            .input(self.yields.clone())
            .input(self.maturity.clone())
            .output("fitted")
            .output("residual")
            .params(h.finish())
    }

    fn compute(&self, t: Ts, universe: &[EntityId], x: &[Vec<f64>]) -> Result<Vec<Vec<f64>>> {
        let n = universe.len();
        let tau: Vec<f64> = (0..n).map(|j| (x[1][j] - t.days() as f64) / 365.25).collect();
        let rows: Vec<([f64; 3], f64)> = (0..n).filter(|&j| x[0][j].is_finite() && tau[j] > 0.0).map(|j| (ns_basis(tau[j], self.lambda), x[0][j])).collect();
        let mut fitted = vec![f64::NAN; n];
        let mut residual = vec![f64::NAN; n];
        if rows.len() >= self.min_points.max(3)
            && let Some(beta) = least_squares3(&rows)
        {
            for j in 0..n {
                if tau[j] > 0.0 {
                    let b = ns_basis(tau[j], self.lambda);
                    fitted[j] = beta[0] * b[0] + beta[1] * b[1] + beta[2] * b[2];
                    residual[j] = x[0][j] - fitted[j];
                }
            }
        }
        Ok(vec![fitted, residual])
    }
}

/// Exponentially weighted volatility of log changes in a level, annualised.
#[derive(Clone, Debug)]
pub struct EwmaVol {
    /// Model name.
    pub name: String,
    /// The level (a price, an equity value).
    pub input: Input,
    /// Half-life, in observations.
    pub halflife: f64,
    /// Observations per year.
    pub annualise: f64,
    /// Observations before the first output.
    pub warmup: usize,
}

impl TimeSeriesModel for EwmaVol {
    fn spec(&self) -> ModelSpec {
        let mut h = Hash64::new();
        h.f64(self.halflife);
        h.f64(self.annualise);
        h.u64(self.warmup as u64);
        ModelSpec::new(&self.name, Shape::TimeSeries).input(self.input.clone()).output("vol").params(h.finish())
    }

    fn state(&self) -> Vec<f64> {
        vec![f64::NAN, 0.0, 0.0] // last level, variance, count
    }

    fn step(&self, _t: Ts, x: &[f64], s: &mut Vec<f64>) -> Result<Vec<f64>> {
        let lambda = 0.5f64.powf(1.0 / self.halflife);
        if x[0].is_finite() && x[0] > 0.0 {
            if s[0].is_finite() {
                let r = (x[0] / s[0]).ln();
                s[1] = if s[2] == 0.0 { r * r } else { lambda * s[1] + (1.0 - lambda) * r * r };
                s[2] += 1.0;
            }
            s[0] = x[0];
        }
        Ok(if s[2] as usize >= self.warmup.max(1) { vec![(s[1] * self.annualise).sqrt()] } else { vec![f64::NAN] })
    }
}

/// The Merton model's solution for one issuer at one date.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct MertonSolution {
    /// Asset value.
    pub assets: f64,
    /// Asset volatility.
    pub asset_vol: f64,
    /// Distance to default.
    pub dd: f64,
    /// Default probability over the horizon, N(−DD).
    pub pd: f64,
    /// Credit spread, −ln(N(d₂) + (V / D e^(−rT)) N(−d₁)) / T.
    pub spread: f64,
}

/// Solves the Merton model: equity `e` is a call on assets with strike debt `d`, rate `r`, equity
/// volatility `sigma_e`, horizon `t` years. Returns `None` if the inputs are not usable or the
/// iteration does not converge.
#[must_use]
pub fn merton(e: f64, d: f64, r: f64, sigma_e: f64, t: f64) -> Option<MertonSolution> {
    if !(e > 0.0 && d > 0.0 && sigma_e > 0.0 && t > 0.0 && r.is_finite()) {
        return None;
    }
    let dpv = d * (-r * t).exp();
    let d12 = |v: f64, sv: f64| {
        let d1 = ((v / d).ln() + (r + 0.5 * sv * sv) * t) / (sv * t.sqrt());
        (d1, d1 - sv * t.sqrt())
    };
    let mut v = e + dpv;
    let mut sv = sigma_e * e / v;
    let mut converged = false;
    for _ in 0..500 {
        for _ in 0..100 {
            let (d1, d2) = d12(v, sv);
            let f = v * norm_cdf(d1) - dpv * norm_cdf(d2) - e;
            let step = f / norm_cdf(d1).max(1e-300);
            v = (v - step).max(e * 1e-6);
            if step.abs() <= 1e-12 * v.max(1.0) {
                break;
            }
        }
        let (d1, _) = d12(v, sv);
        let next = sigma_e * e / (v * norm_cdf(d1)).max(1e-300);
        if (next - sv).abs() <= 1e-12 {
            sv = next;
            converged = true;
            break;
        }
        sv = next;
    }
    if !converged || !v.is_finite() || !sv.is_finite() {
        return None;
    }
    let (d1, d2) = d12(v, sv);
    let dd = ((v / d).ln() + (r - 0.5 * sv * sv) * t) / (sv * t.sqrt());
    let spread = -(norm_cdf(d2) + v / dpv * norm_cdf(-d1)).ln() / t;
    Some(MertonSolution { assets: v, asset_vol: sv, dd, pd: norm_cdf(-dd), spread })
}

/// The Merton model per issuer: equity value (daily), total debt (as of the latest filing known)
/// and a market-wide rate. Equity volatility is estimated from a rolling window of log returns.
#[derive(Clone, Debug)]
pub struct Merton {
    /// Model name.
    pub name: String,
    /// Equity market value.
    pub equity: Input,
    /// Debt (default point), as of the latest filing known.
    pub debt: Input,
    /// Risk-free rate (fraction), market-wide.
    pub rate: Input,
    /// Horizon in years.
    pub horizon: f64,
    /// Returns in the volatility window.
    pub window: usize,
    /// Observations per year.
    pub annualise: f64,
}

impl TimeSeriesModel for Merton {
    fn spec(&self) -> ModelSpec {
        let mut h = Hash64::new();
        h.f64(self.horizon);
        h.u64(self.window as u64);
        h.f64(self.annualise);
        ModelSpec::new(&self.name, Shape::TimeSeries)
            .input(self.equity.clone())
            .input(self.debt.clone())
            .input(self.rate.clone())
            .output("dd")
            .output("pd")
            .output("spread")
            .params(h.finish())
    }

    fn state(&self) -> Vec<f64> {
        // [last equity, count, position, sum, sum of squares, buffer…]
        let mut s = vec![f64::NAN, 0.0, 0.0, 0.0, 0.0];
        s.extend(std::iter::repeat_n(0.0, self.window.max(2)));
        s
    }

    fn step(&self, _t: Ts, x: &[f64], s: &mut Vec<f64>) -> Result<Vec<f64>> {
        let w = self.window.max(2);
        let (e, d, r) = (x[0], x[1], x[2]);
        if e.is_finite() && e > 0.0 {
            if s[0].is_finite() {
                let ret = (e / s[0]).ln();
                let pos = s[2] as usize;
                let old = s[5 + pos];
                if s[1] as usize >= w {
                    s[3] -= old;
                    s[4] -= old * old;
                } else {
                    s[1] += 1.0;
                }
                s[5 + pos] = ret;
                s[3] += ret;
                s[4] += ret * ret;
                s[2] = ((pos + 1) % w) as f64;
            }
            s[0] = e;
        }
        let n = s[1];
        if (n as usize) < w {
            return Ok(vec![f64::NAN; 3]);
        }
        let var = ((s[4] - s[3] * s[3] / n) / (n - 1.0)).max(0.0);
        let sigma_e = (var * self.annualise).sqrt();
        Ok(match merton(e, d, r, sigma_e, self.horizon) {
            Some(m) => vec![m.dd, m.pd, m.spread],
            None => vec![f64::NAN; 3],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merton_reprices_equity_and_volatility() {
        let (e, d, r, se, t) = (100.0, 80.0, 0.03, 0.4, 1.0);
        let m = merton(e, d, r, se, t).expect("converges");
        let dpv = d * (-r * t).exp();
        let d1 = ((m.assets / d).ln() + (r + 0.5 * m.asset_vol.powi(2)) * t) / (m.asset_vol * t.sqrt());
        let d2 = d1 - m.asset_vol * t.sqrt();
        let e_model = m.assets * norm_cdf(d1) - dpv * norm_cdf(d2);
        assert!((e_model - e).abs() < 1e-8, "equity {e_model}");
        assert!((m.asset_vol * m.assets * norm_cdf(d1) - se * e).abs() < 1e-8);
        assert!(m.pd > 0.0 && m.pd < 1.0 && m.spread > 0.0 && m.dd > 0.0);
    }

    #[test]
    fn nelson_siegel_recovers_its_own_curve() {
        let beta = [0.04, -0.02, 0.01];
        let rows: Vec<([f64; 3], f64)> = (1..40)
            .map(|i| {
                let tau = i as f64 * 0.75;
                let b = ns_basis(tau, 1.5);
                (b, beta[0] * b[0] + beta[1] * b[1] + beta[2] * b[2])
            })
            .collect();
        let fit = least_squares3(&rows).unwrap();
        for (a, b) in fit.iter().zip(beta) {
            assert!((a - b).abs() < 1e-10);
        }
    }

    #[test]
    fn zscore_within_groups() {
        let z = ZScore { name: "z".into(), input: Input::exact("s", "v"), group: Some(Input::exact("s", "g")), ddof: 0.0, min_count: 2 };
        let u = [EntityId(0), EntityId(1), EntityId(2), EntityId(3)];
        let out = z.compute(Ts(0), &u, &[vec![1.0, 3.0, 10.0, 20.0], vec![0.0, 0.0, 1.0, 1.0]]).unwrap();
        assert_eq!(out[0], vec![-1.0, 1.0, -1.0, 1.0]);
    }
}
