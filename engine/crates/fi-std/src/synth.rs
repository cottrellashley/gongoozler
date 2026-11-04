//! A synthetic fixed income universe for tests and speed tests.
//!
//! * Bonds: clean marks (mean-reverting to par), accrued interest, semi-annual coupons. Some
//!   amortise (2% of face a quarter, at par) and some pay in kind. Marks are published the next
//!   morning; a fraction can be left missing to exercise the missing-data policies.
//! * A signal and a sector per bond, and an issuer per bond (a relationship series).
//! * An index level.
//! * Issuers: equity values, and debt filed 45 days after each quarter end.
//! * A market-wide rate.
//! * Government bonds yielding on a Nelson–Siegel curve plus noise, with their maturities.

use std::sync::Arc;

use fi_kernel::{Calendar, Column, Contract, EntityId, Result, Series, Ts, Unit, Workspace, time::days_from_civil};

use crate::contracts::{SuppliedBond, TotalReturn};
use crate::models::ns_basis;

/// SplitMix64: small, fast, reproducible.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    /// A generator from a seed.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    /// Next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in (0, 1).
    pub fn uniform(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }

    /// Standard normal (Box–Muller).
    pub fn normal(&mut self) -> f64 {
        let (u, v) = (self.uniform(), self.uniform());
        (-2.0 * u.ln()).sqrt() * (2.0 * std::f64::consts::PI * v).cos()
    }
}

/// What to generate.
#[derive(Clone, Debug)]
pub struct Config {
    /// Number of bonds.
    pub bonds: usize,
    /// Number of weekdays.
    pub days: usize,
    /// Seed.
    pub seed: u64,
    /// Every n-th bond amortises (0: none).
    pub amortising_every: usize,
    /// Every n-th bond pays in kind (0: none).
    pub pik_every: usize,
    /// Fraction of marks left missing.
    pub gap_rate: f64,
    /// Number of issuers.
    pub issuers: usize,
    /// Number of government bonds.
    pub govts: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self { bonds: 200, days: 520, seed: 7, amortising_every: 5, pik_every: 11, gap_rate: 0.0, issuers: 20, govts: 30 }
    }
}

/// The generated entities.
#[derive(Clone, Debug)]
pub struct Universe {
    /// Weekdays.
    pub calendar: Calendar,
    /// Bonds.
    pub bonds: Vec<EntityId>,
    /// Issuers.
    pub issuers: Vec<EntityId>,
    /// Government bonds.
    pub govts: Vec<EntityId>,
    /// The index.
    pub index: EntityId,
    /// Missing marks generated.
    pub gaps: usize,
}

impl Universe {
    /// One [`SuppliedBond`] per bond.
    #[must_use]
    pub fn bond_contracts(&self) -> Vec<Arc<dyn Contract>> {
        self.bonds
            .iter()
            .map(|e| {
                Arc::new(SuppliedBond {
                    entity: *e,
                    marks: "marks".into(),
                    clean: "clean".into(),
                    accrued: "accrued".into(),
                    flows: Some("flows".into()),
                    factor: None,
                    reconcile: true,
                }) as Arc<dyn Contract>
            })
            .collect()
    }

    /// The index as a tradable level.
    #[must_use]
    pub fn index_contract(&self) -> Arc<dyn Contract> {
        Arc::new(TotalReturn { entity: self.index, series: "index".into(), column: "level".into() })
    }
}

const PUBLISH: i64 = 18 * 3_600_000_000_000; // marks are published the next morning

/// Generates the universe into `ws`.
///
/// # Errors
/// If a series name is already used.
pub fn build(ws: &mut Workspace, cfg: &Config) -> Result<Universe> {
    let mut rng = Rng::new(cfg.seed);
    let start = Ts::from_days(days_from_civil(2016, 1, 4));
    let calendar = Calendar::weekdays(start, Ts::from_days(start.days() + (cfg.days as i64) * 7 / 5 + 7));
    let calendar = calendar.prefix(cfg.days);
    let times = calendar.times().to_vec();
    let (tn, n) = (times.len(), cfg.bonds);

    let bonds: Vec<EntityId> = (0..n).map(|j| ws.entities.intern("bond", &format!("B{j:06}"))).collect();
    let issuers: Vec<EntityId> = (0..cfg.issuers.max(1)).map(|j| ws.entities.intern("issuer", &format!("I{j:04}"))).collect();
    let govts: Vec<EntityId> = (0..cfg.govts).map(|j| ws.entities.intern("govt", &format!("G{j:03}"))).collect();
    let index = ws.entities.intern("index", "IDX");

    // Bonds: marks, accrued, signal (grid) and flows (rows).
    let mut clean = vec![0.0; tn * n];
    let mut accrued = vec![0.0; tn * n];
    let mut signal = vec![0.0; tn * n];
    let (mut fe, mut fv, mut fk) = (Vec::new(), Vec::new(), Vec::new());
    let (mut f_int, mut f_face, mut f_price, mut f_pik) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut gaps = 0;
    for (j, &bond) in bonds.iter().enumerate() {
        let coupon = 0.02 + 0.06 * rng.uniform();
        let amortises = cfg.amortising_every > 0 && j % cfg.amortising_every == 0;
        let pik = cfg.pik_every > 0 && j % cfg.pik_every == 1;
        let offset = j % 126;
        let mut px = 100.0 + 5.0 * rng.normal();
        let mut balance = 1.0f64;
        let mut last_coupon = times[0];
        let mut sig = rng.normal();
        for (ti, t) in times.iter().enumerate() {
            if ti > 0 {
                px += 0.002 * (100.0 - px) + 0.25 * rng.normal();
                sig = 0.95 * sig + 0.31 * rng.normal();
            }
            let pays = ti > 0 && (ti + offset) % 126 == 0;
            let amort = amortises && ti > 0 && (ti + offset) % 63 == 0;
            if pays || amort {
                fe.push(bond);
                fv.push(*t);
                fk.push(t.plus(Ts::DAY));
                let half = coupon / 2.0 * balance;
                f_int.push(if pays && !pik { half } else { 0.0 });
                f_pik.push(if pays && pik { half } else { 0.0 });
                let face = if amort { 0.02 * balance } else { 0.0 };
                f_face.push(face);
                f_price.push(100.0);
                balance = balance - face + if pays && pik { half } else { 0.0 };
            }
            if pays {
                last_coupon = *t;
            }
            let at = ti * n + j;
            clean[at] = px;
            accrued[at] = coupon * 100.0 * (t.0 - last_coupon.0) as f64 / (365.0 * Ts::DAY as f64);
            signal[at] = sig;
            if ti > 0 && cfg.gap_rate > 0.0 && rng.uniform() < cfg.gap_rate {
                clean[at] = f64::NAN;
                gaps += 1;
            }
        }
    }
    let known: Vec<Ts> = times.iter().map(|t| t.plus(PUBLISH)).collect();
    ws.add_series(Series::grid(
        "marks",
        bonds.clone(),
        times.clone(),
        known.clone(),
        vec![Column::new("clean", Unit::Price, clean), Column::new("accrued", Unit::Price, accrued), Column::new("signal", Unit::Number, signal)],
    )?)?;
    ws.add_series(Series::rows(
        "flows",
        fe,
        fv,
        fk,
        vec![
            Column::new("interest", Unit::Number, f_int),
            Column::new("principal_face", Unit::Number, f_face),
            Column::new("principal_price", Unit::Price, f_price),
            Column::new("pik_face", Unit::Number, f_pik),
        ],
    )?)?;

    // Relationships and categories, dated at the start.
    let t0 = vec![times[0]; n];
    ws.add_series(Series::rows(
        "issuer",
        bonds.clone(),
        t0.clone(),
        t0.clone(),
        vec![Column::new("issuer", Unit::Ref, (0..n).map(|j| issuers[j % issuers.len()].as_value()).collect())],
    )?)?;
    ws.add_series(Series::rows("sector", bonds.clone(), t0.clone(), t0, vec![Column::new("code", Unit::Category, (0..n).map(|j| (j % 5) as f64).collect())])?)?;

    // The index.
    let mut level = Vec::with_capacity(tn);
    let mut l = 100.0;
    for ti in 0..tn {
        if ti > 0 {
            l *= 1.0 + 0.0002 + 0.003 * rng.normal();
        }
        level.push(l);
    }
    ws.add_series(Series::grid("index", vec![index], times.clone(), known.clone(), vec![Column::new("level", Unit::Price, level)])?)?;

    // Issuers: equity (grid) and debt filed 45 days after each quarter end (rows).
    let ni = issuers.len();
    let mut equity = vec![0.0; tn * ni];
    let (mut de, mut dv, mut dk, mut debt) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for (i, issuer) in issuers.iter().enumerate() {
        let mut e = 50.0 + 100.0 * rng.uniform();
        let mut d = e * (0.5 + rng.uniform());
        for (ti, t) in times.iter().enumerate() {
            if ti > 0 {
                e *= (0.3 / 252f64.sqrt() * rng.normal()).exp();
            }
            equity[ti * ni + i] = e;
            if ti % 63 == 0 {
                d *= 1.0 + 0.05 * rng.normal();
                de.push(*issuer);
                dv.push(*t);
                dk.push(t.plus(45 * Ts::DAY));
                debt.push(d);
            }
        }
    }
    ws.add_series(Series::grid("equity", issuers.clone(), times.clone(), known.clone(), vec![Column::new("value", Unit::Number, equity)])?)?;
    ws.add_series(Series::rows("financials", de, dv, dk, vec![Column::new("total_debt", Unit::Number, debt)])?)?;

    // A market-wide rate, published the next morning.
    let rates: Vec<f64> = (0..tn).map(|ti| 0.03 + 0.005 * (ti as f64 / 250.0).sin()).collect();
    ws.add_series(Series::rows("rate", vec![EntityId::NONE; tn], times.clone(), known.clone(), vec![Column::new("value", Unit::Rate, rates)])?)?;

    // Government bonds on a Nelson–Siegel curve.
    let ng = govts.len();
    if ng > 0 {
        let maturities: Vec<f64> = (0..ng).map(|g| times[0].days() as f64 + 365.25 * (0.5 + 30.0 * g as f64 / ng as f64)).collect();
        let mut yields = vec![0.0; tn * ng];
        let mut mats = vec![0.0; tn * ng];
        for (ti, t) in times.iter().enumerate() {
            let beta = [0.035 + 0.005 * (ti as f64 / 300.0).sin(), -0.015, 0.01];
            for g in 0..ng {
                let tau = (maturities[g] - t.days() as f64) / 365.25;
                let b = ns_basis(tau.max(1e-6), 1.8);
                yields[ti * ng + g] = beta[0] * b[0] + beta[1] * b[1] + beta[2] * b[2] + 0.0002 * rng.normal();
                mats[ti * ng + g] = maturities[g];
            }
        }
        ws.add_series(Series::grid(
            "govt",
            govts.clone(),
            times.clone(),
            known,
            vec![Column::new("yield", Unit::Rate, yields), Column::new("maturity", Unit::Number, mats)],
        )?)?;
    }
    Ok(Universe { calendar, bonds, issuers, govts, index, gaps })
}
