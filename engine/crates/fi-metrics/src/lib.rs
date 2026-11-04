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
    fn a_portfolio_against_itself() {
        let rets: Vec<f64> = (0..200).map(|i| 0.01 * ((i * 31 % 17) as f64 / 17.0 - 0.45)).collect();
        let l = ledger(&rets);
        let rep = report(&l, Some(&l), 252);
        assert!(rep.tracking_error.unwrap().abs() < 1e-12);
        assert!((rep.up_capture.unwrap() - 1.0).abs() < 1e-12 && (rep.down_capture.unwrap() - 1.0).abs() < 1e-12);
    }
}
