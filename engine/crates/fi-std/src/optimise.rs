//! A portfolio optimiser behind the decision interface (review M6; the brief's §8): at each
//! decision, the weights that maximise expected return less risk and turnover, under declared
//! constraints, with the solver's status and the constraints that bind written to the journal.
//!
//! ```text
//! maximise    Σ αᵢwᵢ − λ Σ σᵢ²wᵢ² − κ Σ (wᵢ − w⁰ᵢ)²
//! subject to  lo ≤ wᵢ ≤ hi              (weight bounds)
//!             Σ wᵢ = gross               (the budget)
//!             bⱼ ≤ Σ xⱼᵢ wᵢ ≤ Bⱼ          (exposure bands in any per-unit measure: duration, spread duration)
//!             Σ_{i ∈ g} wᵢ ≤ cap          (group caps: sector, rating, issuer)
//! ```
//!
//! The solver is ADMM on the quadratic programme, as OSQP solves it: the objective's matrix is
//! diagonal and the constraints are the identity (the bounds) plus a few dense rows, so each
//! iteration is a diagonal solve and an m × m one (the Woodbury identity), linear in the universe.
//! It stops when the primal and dual residuals are below tolerance, says so, and names the
//! constraints that bind; when the constraints cannot all hold, it says that too, and the
//! decision keeps the book or fails, as declared. Nothing is relaxed silently.

use fi_kernel::{DecisionCtx, DecisionModel, EntityId, Error, Result, Targets, hash::Hash64};

/// How the solve ended.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Converged: every constraint holds to tolerance, and the solution is optimal.
    Solved,
    /// The iteration limit came first: the best point found, constraints within the residual.
    MaxIterations,
    /// No weights satisfy every constraint.
    Infeasible,
}

/// A solution.
#[derive(Clone, Debug)]
pub struct Solution {
    /// The weights.
    pub w: Vec<f64>,
    /// How the solve ended.
    pub status: Status,
    /// Iterations taken.
    pub iterations: usize,
    /// The constraints at a bound with a non-zero multiplier, by name.
    pub binding: Vec<String>,
}

/// A linear constraint row: `lo ≤ a·w ≤ hi`, with its name.
#[derive(Clone, Debug)]
pub struct Row {
    /// Coefficients, one per variable.
    pub a: Vec<f64>,
    /// Lower bound (−∞ for none).
    pub lo: f64,
    /// Upper bound (+∞ for none).
    pub hi: f64,
    /// What it is, for the journal.
    pub name: String,
}

/// Solves `minimise ½ Σ pᵢwᵢ² + Σ qᵢwᵢ` over `lo ≤ w ≤ hi` and the `rows`, `p` ≥ 0.
#[must_use]
#[allow(clippy::many_single_char_names)]
pub fn solve(p: &[f64], q: &[f64], lo: &[f64], hi: &[f64], rows: &[Row]) -> Solution {
    let (n, m) = (p.len(), rows.len());
    let (rho, sigma) = (1.0, 1e-6);
    let (eps_abs, eps_rel, eps_inf) = (1e-9, 1e-9, 1e-7);
    let max_iter = 50_000;
    // D = P + σI + ρI; K = I/ρ + R D⁻¹ Rᵀ, inverted once.
    let d: Vec<f64> = p.iter().map(|pi| pi + sigma + rho).collect();
    let mut k = vec![0.0; m * m];
    for i in 0..m {
        for j in 0..m {
            k[i * m + j] = (0..n).map(|c| rows[i].a[c] * rows[j].a[c] / d[c]).sum::<f64>() + if i == j { 1.0 / rho } else { 0.0 };
        }
    }
    let kinv = invert(&k, m);
    let (mut x, mut z, mut y) = (vec![0.0; n], vec![0.0; n + m], vec![0.0; n + m]);
    let (lower, upper): (Vec<f64>, Vec<f64>) = {
        let mut l = lo.to_vec();
        let mut u = hi.to_vec();
        l.extend(rows.iter().map(|r| r.lo));
        u.extend(rows.iter().map(|r| r.hi));
        (l, u)
    };
    let ax = |x: &[f64]| -> Vec<f64> {
        let mut v = x.to_vec();
        v.extend(rows.iter().map(|r| r.a.iter().zip(x).map(|(a, b)| a * b).sum::<f64>()));
        v
    };
    let at = |v: &[f64]| -> Vec<f64> {
        let mut out = v[..n].to_vec();
        for (j, r) in rows.iter().enumerate() {
            for (o, a) in out.iter_mut().zip(&r.a) {
                *o += a * v[n + j];
            }
        }
        out
    };
    let mut status = Status::MaxIterations;
    let mut iterations = max_iter;
    for it in 1..=max_iter {
        // x̃ = M⁻¹ (σx − q + Aᵀ(ρz − y)), M = D + ρRᵀR, by Woodbury.
        let rhs_v: Vec<f64> = z.iter().zip(&y).map(|(zi, yi)| rho * zi - yi).collect();
        let atv = at(&rhs_v);
        let b: Vec<f64> = (0..n).map(|i| sigma * x[i] - q[i] + atv[i]).collect();
        let dinv_b: Vec<f64> = b.iter().zip(&d).map(|(bi, di)| bi / di).collect();
        let r_dinv_b: Vec<f64> = rows.iter().map(|r| r.a.iter().zip(&dinv_b).map(|(a, v)| a * v).sum()).collect();
        let t: Vec<f64> = (0..m).map(|i| (0..m).map(|j| kinv[i * m + j] * r_dinv_b[j]).sum()).collect();
        let xt: Vec<f64> = (0..n).map(|c| dinv_b[c] - rows.iter().zip(&t).map(|(r, tj)| r.a[c] * tj).sum::<f64>() / d[c]).collect();
        let zt = ax(&xt);
        let y_old = y.clone();
        for i in 0..n + m {
            z[i] = (zt[i] + y[i] / rho).clamp(lower[i], upper[i]);
            y[i] += rho * (zt[i] - z[i]);
        }
        x = xt;
        // Residuals: primal ‖Ax − z‖∞, dual ‖Px + q + Aᵀy‖∞.
        let axv = ax(&x);
        let r_prim = axv.iter().zip(&z).map(|(a, b)| (a - b).abs()).fold(0.0, f64::max);
        let aty = at(&y);
        let r_dual = (0..n).map(|i| (p[i] * x[i] + q[i] + aty[i]).abs()).fold(0.0, f64::max);
        let norm = |v: &[f64]| v.iter().map(|x| x.abs()).fold(0.0, f64::max);
        let tol_p = eps_abs + eps_rel * norm(&axv).max(norm(&z));
        let tol_d = eps_abs + eps_rel * norm(&aty).max(norm(q));
        if r_prim <= tol_p && r_dual <= tol_d {
            status = Status::Solved;
            iterations = it;
            break;
        }
        // A certificate of primal infeasibility (OSQP): δy with Aᵀδy ≈ 0 and uᵀδy⁺ + lᵀδy⁻ < 0.
        if it % 25 == 0 {
            let dy: Vec<f64> = y.iter().zip(&y_old).map(|(a, b)| a - b).collect();
            let ndy = norm(&dy);
            if ndy > 0.0 {
                let atdy = at(&dy);
                let support: f64 = (0..n + m)
                    .map(|i| {
                        if dy[i] > 0.0 {
                            upper[i] * dy[i]
                        } else if dy[i] < 0.0 {
                            lower[i] * dy[i]
                        } else {
                            0.0
                        }
                    })
                    .filter(|v| v.is_finite())
                    .sum();
                if norm(&atdy) <= eps_inf * ndy && support < -eps_inf * ndy {
                    status = Status::Infeasible;
                    iterations = it;
                    break;
                }
            }
        }
    }
    let mut binding = Vec::new();
    if status != Status::Infeasible {
        let tol = 1e-7;
        let (mut at_lo, mut at_hi) = (0, 0);
        for i in 0..n {
            if y[i].abs() > tol {
                if (z[i] - lower[i]).abs() < tol {
                    at_lo += 1;
                } else if (z[i] - upper[i]).abs() < tol {
                    at_hi += 1;
                }
            }
        }
        if at_hi > 0 {
            binding.push(format!("{at_hi} at their maximum weight"));
        }
        if at_lo > 0 && lower.iter().take(n).any(|l| *l != 0.0) {
            binding.push(format!("{at_lo} at their minimum weight"));
        }
        for (j, r) in rows.iter().enumerate() {
            let (zj, yj) = (z[n + j], y[n + j]);
            if yj.abs() > tol && ((zj - r.lo).abs() < tol || (zj - r.hi).abs() < tol) {
                binding.push(r.name.clone());
            }
        }
    }
    Solution { w: x, status, iterations, binding }
}

/// Gauss–Jordan inverse of an m × m matrix (m is the few dense constraints).
fn invert(a: &[f64], m: usize) -> Vec<f64> {
    let mut a = a.to_vec();
    let mut inv: Vec<f64> = (0..m * m).map(|i| if i / m == i % m { 1.0 } else { 0.0 }).collect();
    for c in 0..m {
        let piv = (c..m).max_by(|&i, &j| a[i * m + c].abs().total_cmp(&a[j * m + c].abs())).unwrap_or(c);
        for j in 0..m {
            a.swap(c * m + j, piv * m + j);
            inv.swap(c * m + j, piv * m + j);
        }
        let d = a[c * m + c];
        for j in 0..m {
            a[c * m + j] /= d;
            inv[c * m + j] /= d;
        }
        for i in 0..m {
            if i != c {
                let f = a[i * m + c];
                for j in 0..m {
                    a[i * m + j] -= f * a[c * m + j];
                    inv[i * m + j] -= f * inv[c * m + j];
                }
            }
        }
    }
    inv
}

/// A band on a portfolio exposure: `min ≤ Σ wᵢxᵢ ≤ max`, `x` a per-contract value known at the
/// decision (a duration, a spread duration).
#[derive(Clone, Debug, PartialEq)]
pub struct Exposure {
    /// Its series.
    pub series: String,
    /// Its column.
    pub column: String,
    /// The least.
    pub min: f64,
    /// The most.
    pub max: f64,
}

/// A cap on each group's weight: the group a contract is in is a per-contract value known at the
/// decision (a sector or rating code).
#[derive(Clone, Debug, PartialEq)]
pub struct GroupCap {
    /// Its series.
    pub series: String,
    /// Its column.
    pub column: String,
    /// The most each group may weigh.
    pub cap: f64,
}

/// What a decision does when no weights meet its constraints.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum OnInfeasible {
    /// Keep the book, and say why in the journal.
    #[default]
    Keep,
    /// Stop the run.
    Fail,
}

/// The optimiser as a decision.
#[derive(Clone, Debug)]
pub struct Optimise {
    /// Expected return per contract: (series, column).
    pub alpha: (String, String),
    /// Risk (a volatility) per contract, or one for each.
    pub risk: Option<(String, String)>,
    /// λ, the penalty on risk.
    pub risk_aversion: f64,
    /// κ, the penalty on squared changes of weight.
    pub turnover: f64,
    /// The weights' sum.
    pub gross: f64,
    /// Each weight's bounds.
    pub bounds: (f64, f64),
    /// Exposure bands.
    pub exposures: Vec<Exposure>,
    /// Group caps.
    pub groups: Option<GroupCap>,
    /// Candidates.
    pub universe: Vec<EntityId>,
    /// When no weights meet the constraints.
    pub on_infeasible: OnInfeasible,
}

impl DecisionModel for Optimise {
    fn name(&self) -> String {
        format!("optimise({}.{})", self.alpha.0, self.alpha.1)
    }

    fn params(&self) -> u64 {
        let mut h = Hash64::new();
        h.str("optimise");
        h.str(&format!("{:?}", (&self.alpha, &self.risk, &self.exposures, &self.groups, self.on_infeasible)));
        for x in [self.risk_aversion, self.turnover, self.gross, self.bounds.0, self.bounds.1] {
            h.f64(x);
        }
        for e in &self.universe {
            h.u64(u64::from(e.0));
        }
        h.finish()
    }

    fn decide(&self, ctx: &DecisionCtx<'_>) -> Result<Option<Targets>> {
        let read = |(s, c): (&str, &str)| -> Result<Vec<f64>> {
            let series = ctx.view.series(s)?;
            let col = series.column(c)?;
            Ok(ctx.view.cross_section(series, col, &self.universe))
        };
        let alpha = read((&self.alpha.0, &self.alpha.1))?;
        let risk = match &self.risk {
            Some((s, c)) => read((s, c))?,
            None => vec![1.0; self.universe.len()],
        };
        let xs = self.exposures.iter().map(|e| read((&e.series, &e.column))).collect::<Result<Vec<_>>>()?;
        let groups = self.groups.as_ref().map(|g| read((&g.series, &g.column))).transpose()?;
        // Only what can be checked takes part: every input known.
        let ok: Vec<usize> = (0..self.universe.len())
            .filter(|&i| alpha[i].is_finite() && risk[i].is_finite() && xs.iter().all(|x| x[i].is_finite()) && groups.as_ref().is_none_or(|g| g[i].is_finite()))
            .collect();
        if ok.is_empty() {
            return Ok(None);
        }
        // The current weights: the book's value in each candidate over NAV.
        let mut w0 = vec![0.0; ok.len()];
        for &(row, q) in ctx.holdings {
            let e = ctx.records.entities()[row];
            if let Some(j) = ok.iter().position(|&i| self.universe[i] == e) {
                let v = ctx.records.value[ctx.records.at(row, ctx.k)];
                if v.is_finite() && ctx.nav > 0.0 {
                    w0[j] = q * v / ctx.nav;
                }
            }
        }
        let (lam, kap) = (self.risk_aversion, self.turnover);
        let p: Vec<f64> = ok.iter().map(|&i| 2.0 * lam * risk[i] * risk[i] + 2.0 * kap).collect();
        let q: Vec<f64> = ok.iter().zip(&w0).map(|(&i, w)| -alpha[i] - 2.0 * kap * w).collect();
        let n = ok.len();
        let mut rows = vec![Row { a: vec![1.0; n], lo: self.gross, hi: self.gross, name: "the budget".into() }];
        for (e, x) in self.exposures.iter().zip(&xs) {
            rows.push(Row {
                a: ok.iter().map(|&i| x[i]).collect(),
                lo: e.min,
                hi: e.max,
                name: format!("{}.{} in [{}, {}]", e.series, e.column, e.min, e.max),
            });
        }
        if let (Some(g), Some(codes)) = (&self.groups, &groups) {
            let mut seen: Vec<f64> = ok.iter().map(|&i| codes[i]).collect();
            seen.sort_by(f64::total_cmp);
            seen.dedup();
            for code in seen {
                let a = ok.iter().map(|&i| if codes[i] == code { 1.0 } else { 0.0 }).collect();
                rows.push(Row { a, lo: f64::NEG_INFINITY, hi: g.cap, name: format!("{}.{} group {code} ≤ {}", g.series, g.column, g.cap) });
            }
        }
        let sol = solve(&p, &q, &vec![self.bounds.0; n], &vec![self.bounds.1; n], &rows);
        let note = |s: String| {
            if let Ok(mut v) = ctx.notes.lock() {
                v.push(s);
            }
        };
        match sol.status {
            Status::Infeasible => {
                let why = format!("optimiser: infeasible at {} (no weights meet every constraint)", ctx.t);
                return match self.on_infeasible {
                    OnInfeasible::Keep => {
                        note(format!("{why}; kept the book"));
                        Ok(None)
                    }
                    OnInfeasible::Fail => Err(Error::Invalid(why)),
                };
            }
            s => {
                let binding = if sol.binding.is_empty() { "none".to_owned() } else { sol.binding.join("; ") };
                note(format!("optimiser: {s:?} in {} iterations; binding: {binding}", sol.iterations));
            }
        }
        Ok(Some(Targets::Weights(ok.iter().zip(&sol.w).map(|(&i, w)| (self.universe[i], *w)).collect())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The water-filling answer with only the budget and bounds: wᵢ = clip((αᵢ − μ)/pᵢ, lo, hi),
    /// with μ found by bisection so that the weights sum to the budget.
    fn waterfill(p: &[f64], alpha: &[f64], lo: f64, hi: f64, gross: f64) -> Vec<f64> {
        let w = |mu: f64| -> Vec<f64> { p.iter().zip(alpha).map(|(pi, a)| ((a - mu) / pi).clamp(lo, hi)).collect() };
        let (mut a, mut b) = (-100.0, 100.0);
        for _ in 0..200 {
            let mid = 0.5 * (a + b);
            if w(mid).iter().sum::<f64>() > gross { a = mid } else { b = mid }
        }
        w(0.5 * (a + b))
    }

    #[test]
    fn with_a_budget_and_bounds_it_finds_the_water_filling_answer() {
        let alpha: Vec<f64> = (0..40).map(|i| 0.01 + 0.002 * ((i * 17 % 13) as f64)).collect();
        let p: Vec<f64> = (0..40).map(|i| 0.5 + 0.1 * ((i * 7 % 5) as f64)).collect();
        let q: Vec<f64> = alpha.iter().map(|a| -a).collect();
        let budget = Row { a: vec![1.0; 40], lo: 1.0, hi: 1.0, name: "the budget".into() };
        let sol = solve(&p, &q, &[0.0; 40], &[0.05; 40], &[budget]);
        let want = waterfill(&p, &alpha, 0.0, 0.05, 1.0);
        assert_eq!(sol.status, Status::Solved);
        for (a, b) in sol.w.iter().zip(&want) {
            assert!((a - b).abs() < 1e-6, "{a} vs {b}");
        }
        assert!(sol.binding.iter().any(|b| b == "the budget") && sol.binding.iter().any(|b| b.contains("maximum")));
    }

    #[test]
    fn a_band_binds_when_the_unconstrained_answer_breaks_it_and_impossible_bands_are_infeasible() {
        let n = 10;
        let alpha: Vec<f64> = (0..n).map(|i| 0.01 * i as f64).collect();
        let dur: Vec<f64> = (0..n).map(|i| 1.0 + i as f64).collect();
        let q: Vec<f64> = alpha.iter().map(|a| -a).collect();
        let rows = vec![
            Row { a: vec![1.0; n], lo: 1.0, hi: 1.0, name: "the budget".into() },
            Row { a: dur.clone(), lo: 3.0, hi: 4.0, name: "duration in [3, 4]".into() },
        ];
        let sol = solve(&vec![0.2; n], &q, &vec![0.0; n], &vec![1.0; n], &rows);
        let d: f64 = sol.w.iter().zip(&dur).map(|(w, x)| w * x).sum();
        assert_eq!(sol.status, Status::Solved);
        assert!((d - 4.0).abs() < 1e-6 && sol.binding.iter().any(|b| b.starts_with("duration")), "{d} {:?}", sol.binding);
        let impossible = vec![rows[0].clone(), Row { a: dur, lo: 20.0, hi: 30.0, name: "duration in [20, 30]".into() }];
        assert_eq!(solve(&vec![0.2; n], &q, &vec![0.0; n], &vec![1.0; n], &impossible).status, Status::Infeasible);
    }
}
