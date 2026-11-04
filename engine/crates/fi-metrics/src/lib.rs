//! Performance statistics from Nautilus's analysis crate (used as a library), over a run's
//! accounts, alongside the run's P&L split by term.
//!
//! Nautilus's statistics take returns keyed by time (`BTreeMap<UnixNanos, f64>`), compound them
//! into daily bins, and annualise by `period`. The ledger's returns are daily already.

use fi_kernel::LedgerOutput;
use nautilus_analysis::{
    Returns,
    statistic::PortfolioStatistic,
    statistics::{
        cagr::CAGR, down_capture_ratio::DownCaptureRatio, information_ratio::InformationRatio, max_drawdown::MaxDrawdown,
        returns_volatility::ReturnsVolatility, sharpe_ratio::SharpeRatio, sortino_ratio::SortinoRatio, tracking_error::TrackingError,
        up_capture_ratio::UpCaptureRatio,
    },
};
use nautilus_core::UnixNanos;

/// A run's statistics and its P&L by term.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    /// Total return.
    pub total_return: f64,
    /// Annualised Sharpe ratio (no risk-free rate).
    pub sharpe: f64,
    /// Annualised Sortino ratio.
    pub sortino: f64,
    /// Annualised volatility.
    pub volatility: f64,
    /// Maximum drawdown.
    pub max_drawdown: f64,
    /// Compound annual growth rate.
    pub cagr: f64,
    /// Annualised tracking error against the benchmark.
    pub tracking_error: Option<f64>,
    /// Information ratio against the benchmark.
    pub information_ratio: Option<f64>,
    /// Up-capture ratio against the benchmark.
    pub up_capture: Option<f64>,
    /// Down-capture ratio against the benchmark.
    pub down_capture: Option<f64>,
    /// Income over the run.
    pub income: f64,
    /// Price over the run.
    pub price: f64,
    /// Principal over the run.
    pub principal: f64,
    /// Other flows over the run.
    pub other: f64,
    /// Financing over the run (a cost).
    pub financing: f64,
    /// Trading costs over the run.
    pub costs: f64,
    /// The money-weighted return, a year: what the investors earned given when they added and
    /// took money. The time-weighted return (`cagr`) answers what the strategy earned, whatever
    /// the flows: the two differ exactly when flows came before good or bad periods.
    pub money_weighted: f64,
}

/// The money-weighted return (review M4): the rate a year, over ACT/365F years from the first
/// time, at which the investors' flows are worth nothing together: the opening NAV in, each
/// subscription in, each redemption and distribution out, the closing NAV out. NaN when no rate
/// from −99% to 1,000% a year solves it, or the book has no value to start with.
#[must_use]
pub fn money_weighted(out: &LedgerOutput) -> f64 {
    let n = out.nav.len();
    if n < 2 {
        return f64::NAN;
    }
    let at = |k: usize| -> f64 { out.capital.get(k).copied().unwrap_or(0.0) };
    let costs0 = out.costs.first().copied().unwrap_or(0.0);
    // The opening value (before the first step's trades and flows) in, each step's net capital
    // in, the closing NAV out.
    let mut flows: Vec<(f64, f64)> = vec![(0.0, -(out.nav[0] - at(0) + costs0))];
    let t0 = out.times[0].0 as f64;
    let years = |k: usize| (out.times[k].0 as f64 - t0) / (365.0 * 86_400e9);
    for k in 0..n {
        if at(k) != 0.0 {
            flows.push((years(k), -at(k)));
        }
    }
    flows.push((years(n - 1), out.nav[n - 1]));
    if flows[0].1.is_nan() || flows[0].1 >= 0.0 || flows.iter().any(|(_, f)| !f.is_finite()) {
        return f64::NAN;
    }
    let pv = |r: f64| flows.iter().map(|(t, f)| f * (1.0 + r).powf(-t)).sum::<f64>();
    let (mut lo, mut hi) = (-0.99, 10.0);
    let (mut flo, fhi) = (pv(lo), pv(hi));
    if flo.is_nan() || fhi.is_nan() || flo.signum() == fhi.signum() {
        return f64::NAN;
    }
    for _ in 0..200 {
        let mid = 0.5 * (lo + hi);
        let fm = pv(mid);
        if fm == 0.0 || hi - lo < 1e-14 {
            return mid;
        }
        if fm.signum() == flo.signum() {
            lo = mid;
            flo = fm;
        } else {
            hi = mid;
        }
    }
    0.5 * (lo + hi)
}

/// The run's daily returns, keyed by time, as Nautilus's statistics take them.
#[must_use]
pub fn returns(out: &LedgerOutput) -> Returns {
    out.times.iter().zip(out.returns()).skip(1).map(|(t, r)| (UnixNanos::from(u64::try_from(t.0).unwrap_or(0)), r)).collect()
}

/// Statistics for `out`, against `benchmark` if given; `period` is observations per year.
#[must_use]
pub fn report(out: &LedgerOutput, benchmark: Option<&LedgerOutput>, period: usize) -> Report {
    let r = returns(out);
    let p = Some(period);
    let nan = |x: Option<f64>| x.unwrap_or(f64::NAN);
    let b = benchmark.map(returns);
    let rel = |s: &dyn PortfolioStatistic<Item = f64>| b.as_ref().and_then(|b| s.calculate_from_returns_with_benchmark(&r, b));
    let sum = |v: &[f64]| v.iter().sum::<f64>();
    Report {
        total_return: out.total_return(),
        sharpe: nan(SharpeRatio::new(p).calculate_from_returns(&r)),
        sortino: nan(SortinoRatio::new(p).calculate_from_returns(&r)),
        volatility: nan(ReturnsVolatility::new(p).calculate_from_returns(&r)),
        max_drawdown: nan(MaxDrawdown::new().calculate_from_returns(&r)),
        cagr: nan(CAGR::new(p).calculate_from_returns(&r)),
        tracking_error: rel(&TrackingError::new(p)),
        information_ratio: rel(&InformationRatio::new(p)),
        up_capture: rel(&UpCaptureRatio::new(p)),
        down_capture: rel(&DownCaptureRatio::new(p)),
        income: sum(&out.income),
        price: sum(&out.price),
        principal: sum(&out.principal),
        other: sum(&out.other),
        financing: sum(&out.financing),
        costs: sum(&out.costs),
        money_weighted: money_weighted(out),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fi_kernel::Ts;

    fn ledger(rets: &[f64]) -> LedgerOutput {
        let mut nav = vec![100.0];
        for r in rets {
            nav.push(nav.last().unwrap() * (1.0 + r));
        }
        LedgerOutput { times: (0..nav.len()).map(|d| Ts::from_days(19_000 + d as i64)).collect(), nav, ..LedgerOutput::default() }
    }

    #[test]
    fn sharpe_matches_its_definition() {
        let rets: Vec<f64> = (0..300).map(|i| 0.0004 + 0.01 * ((i * 7919 % 101) as f64 / 101.0 - 0.5)).collect();
        let rep = report(&ledger(&rets), None, 252);
        let n = rets.len() as f64;
        let mean = rets.iter().sum::<f64>() / n;
        let sd = (rets.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n - 1.0)).sqrt();
        assert!((rep.sharpe - mean / sd * 252f64.sqrt()).abs() < 1e-9, "{} vs {}", rep.sharpe, mean / sd * 252f64.sqrt());
        assert!(rep.max_drawdown <= 0.0 && rep.volatility > 0.0);
    }

    #[test]
    fn the_money_weighted_return_weighs_the_flows() {
        // A year at +10% with no flows: 10% a year.
        let mut l = ledger(&[0.10]);
        l.times = vec![Ts::from_days(19_000), Ts::from_days(19_365)];
        l.capital = vec![0.0, 0.0];
        assert!((money_weighted(&l) - 0.10).abs() < 1e-12);
        // Half a year at +10%, then 100 more invested, then half a year at −10%: the investors put
        // more in before the fall, so they earned less than the strategy did.
        let times = vec![Ts::from_days(19_000), Ts::from_days(19_000 + 182), Ts::from_days(19_365)];
        let (n1, n2) = (110.0 + 100.0, (110.0 + 100.0) * 0.9);
        let out = LedgerOutput { times, nav: vec![100.0, n1, n2], capital: vec![0.0, 100.0, 0.0], ..LedgerOutput::default() };
        let r = money_weighted(&out);
        let pv = -100.0 - 100.0 * (1.0 + r).powf(-182.0 / 365.0) + n2 * (1.0 + r).powf(-1.0);
        assert!(pv.abs() < 1e-8 && r < -0.01, "{r}: invested before the fall");
    }

    #[test]
    fn a_portfolio_against_itself() {
        let rets: Vec<f64> = (0..200).map(|i| 0.01 * ((i * 31 % 17) as f64 / 17.0 - 0.45)).collect();
        let l = ledger(&rets);
        let rep = report(&l, Some(&l), 252);
        assert!(rep.tracking_error.unwrap().abs() < 1e-12);
        assert!((rep.up_capture.unwrap() - 1.0).abs() < 1e-12 && (rep.down_capture.unwrap() - 1.0).abs() < 1e-12);
    }
}
