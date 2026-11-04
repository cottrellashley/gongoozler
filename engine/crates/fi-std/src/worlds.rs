//! World generators (spec, change 6): stochastic models that write a world's series. Their draws
//! are keyed by world, entity and event ([`Stream`]), so a world is reproducible, its draws do
//! not depend on threads, and policies compared on it share them. Their role is real-world (P):
//! their outputs are a simulated world, and everything downstream carries that intervention.

use fi_kernel::{
    BatchModel, Calendar, Column, EntityId, Error, ModelRole, ModelSpec, Result, Series, Shape, Ts, Unit, Workspace, hash::Hash64, random::Stream,
};

use crate::models::norm_cdf;

const YEAR_DAYS: f64 = 365.25;

/// Correlated Gaussian paths over the calendar: an Ornstein–Uhlenbeck process (a random walk
/// with drift when `reversion` is 0) for each entity, whose shocks load `correlation` on one
/// common factor. Writes a grid series `name` with column `value`.
#[derive(Clone, Debug)]
pub struct GaussianPaths {
    /// Name: the series it writes.
    pub name: String,
    /// Entities (`EntityId::NONE` for a market-wide series), each with its value at the first
    /// time.
    pub start: Vec<(EntityId, f64)>,
    /// The long-run level (with `reversion` > 0).
    pub mean: f64,
    /// Speed of reversion per year.
    pub reversion: f64,
    /// Drift per year (without reversion).
    pub drift: f64,
    /// Volatility per square-root year.
    pub vol: f64,
    /// Each entity's shocks' correlation with the common factor.
    pub correlation: f64,
    /// The lowest value (a rate floor).
    pub floor: Option<f64>,
    /// The seed.
    pub seed: u64,
    /// The world.
    pub world: u64,
}

impl BatchModel for GaussianPaths {
    fn spec(&self) -> ModelSpec {
        let mut s = ModelSpec::new(&self.name, Shape::Batch);
        s.writes = vec![self.name.clone()];
        s.role = ModelRole::RealWorld;
        let mut h = Hash64::new();
        h.str(&format!("{self:?}"));
        s.params = h.finish();
        s
    }

    fn evaluate(&self, _ws: &Workspace, cal: &Calendar) -> Result<Vec<Series>> {
        let times = cal.times();
        let n = self.start.len();
        if !(0.0..=1.0).contains(&self.correlation) {
            return Err(Error::Invalid(format!("model {}: a correlation is between 0 and 1", self.name)));
        }
        let stream = Stream::new(self.seed, &self.name);
        let (a, b) = (self.correlation.sqrt(), (1.0 - self.correlation).sqrt());
        let mut x: Vec<f64> = self.start.iter().map(|(_, v)| *v).collect();
        let mut values = Vec::with_capacity(times.len() * n);
        values.extend_from_slice(&x);
        for k in 1..times.len() {
            let dt = (times[k].0 - times[k - 1].0) as f64 / Ts::DAY as f64 / YEAR_DAYS;
            let common = stream.normal(self.world, 0, 0, times[k].0);
            for (j, (e, _)) in self.start.iter().enumerate() {
                let z = a * common + b * stream.normal(self.world, 1, u64::from(e.0), times[k].0);
                let pull = if self.reversion > 0.0 { self.reversion * (self.mean - x[j]) } else { self.drift };
                x[j] += pull * dt + self.vol * dt.sqrt() * z;
                if let Some(f) = self.floor {
                    x[j] = x[j].max(f);
                }
            }
            values.extend_from_slice(&x);
        }
        let entities = self.start.iter().map(|(e, _)| *e).collect();
        Ok(vec![Series::grid(self.name.clone(), entities, times.to_vec(), times.to_vec(), vec![Column::new("value", Unit::Number, values)])?])
    }

    fn supports(&self) -> Vec<&'static str> {
        vec!["gaussian paths", "mean reversion", "common factor", "keyed draws"]
    }
}

/// One loan's terms, for [`Defaults`].
#[derive(Clone, Debug, PartialEq)]
pub struct LoanTerms {
    /// The loan (a contract on the flows written).
    pub entity: EntityId,
    /// Its borrower: loans of one borrower default together.
    pub borrower: EntityId,
    /// Coupon per year, Act/365.25, paid on the payment dates.
    pub coupon: f64,
    /// Repaid at par on the first payment date at or after this.
    pub maturity: Ts,
    /// The borrower's default intensity per year.
    pub hazard: f64,
    /// The fraction of face this loan recovers (facilities of one borrower may differ).
    pub recovery: f64,
}

/// Loan cash flows in a world with correlated defaults (a one-factor Gaussian copula): each
/// borrower defaults at an exponential time with its hazard, whose uniforms load
/// `correlation` on a common factor. A default is recognised on the next payment date: no coupon,
/// the recovery in cash, the rest written down. Writes a rows series `name` of `interest`,
/// `principal_face`, `writedown_face` and `defaulted` per unit of original face, for
/// [`SuppliedBond`](crate::SuppliedBond) loans.
#[derive(Clone, Debug)]
pub struct Defaults {
    /// Name: the series it writes.
    pub name: String,
    /// The loans.
    pub loans: Vec<LoanTerms>,
    /// Payment dates: times of the calendar.
    pub payments: Vec<Ts>,
    /// Correlation of the borrowers' default draws with the common factor.
    pub correlation: f64,
    /// The seed.
    pub seed: u64,
    /// The world.
    pub world: u64,
}

impl Defaults {
    /// When each borrower defaults in this world (`Ts::MAX`: not before the end of time).
    #[must_use]
    pub fn default_time(&self, start: Ts, borrower: EntityId, hazard: f64) -> Ts {
        let stream = Stream::new(self.seed, &self.name);
        let (a, b) = (self.correlation.sqrt(), (1.0 - self.correlation).sqrt());
        let z = a * stream.normal(self.world, 0, 0, 0) + b * stream.normal(self.world, 1, u64::from(borrower.0), 0);
        let u = norm_cdf(z).clamp(1e-300, 1.0 - 1e-16);
        if hazard <= 0.0 {
            return Ts::MAX;
        }
        let years = -u.ln() / hazard;
        let days = years * YEAR_DAYS;
        if days > 1e7 { Ts::MAX } else { start.plus((days * Ts::DAY as f64) as i64) }
    }
}

impl BatchModel for Defaults {
    fn spec(&self) -> ModelSpec {
        let mut s = ModelSpec::new(&self.name, Shape::Batch);
        s.writes = vec![self.name.clone()];
        s.role = ModelRole::RealWorld;
        let mut h = Hash64::new();
        h.str(&format!("{self:?}"));
        s.params = h.finish();
        s
    }

    fn evaluate(&self, _ws: &Workspace, cal: &Calendar) -> Result<Vec<Series>> {
        if !(0.0..=1.0).contains(&self.correlation) {
            return Err(Error::Invalid(format!("model {}: a correlation is between 0 and 1", self.name)));
        }
        let start = cal.times().first().copied().ok_or_else(|| Error::Invalid("an empty calendar".into()))?;
        let mut pays = self.payments.clone();
        pays.sort_unstable();
        let (mut e, mut t) = (Vec::new(), Vec::new());
        let mut v = vec![Vec::new(); 4];
        for l in &self.loans {
            let tau = self.default_time(start, l.borrower, l.hazard);
            let mut prev = start;
            for &p in pays.iter().filter(|p| **p > start) {
                let row = if tau <= p {
                    [0.0, l.recovery, 1.0 - l.recovery, 1.0]
                } else {
                    let interest = l.coupon * (p.0 - prev.0) as f64 / Ts::DAY as f64 / YEAR_DAYS;
                    [interest, if p >= l.maturity { 1.0 } else { 0.0 }, 0.0, 0.0]
                };
                e.push(l.entity);
                t.push(p);
                for (c, x) in row.iter().enumerate() {
                    v[c].push(*x);
                }
                prev = p;
                if tau <= p || p >= l.maturity {
                    break;
                }
            }
        }
        let names = ["interest", "principal_face", "writedown_face", "defaulted"];
        let cols = names.iter().zip(v).map(|(n, x)| Column::new(*n, if *n == "defaulted" { Unit::Flag } else { Unit::Number }, x)).collect();
        Ok(vec![Series::rows(self.name.clone(), e, t.clone(), t, cols)?])
    }

    fn supports(&self) -> Vec<&'static str> {
        vec!["correlated defaults", "shared borrowers", "facility recoveries", "keyed draws"]
    }
}
