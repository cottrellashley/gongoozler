//! A run's accounts (`fi.Run`): NAV, cash, P&L by term, orders, holdings, the journal and statistics.

use super::*;

/// A run's accounts: NAV, cash, P&L by term, costs, turnover, holdings.
#[pyclass(name = "Run", module = "fi_engine._native", frozen)]
pub struct PyRun {
    pub(crate) name: String,
    pub(crate) out: LedgerOutput,
    pub(crate) names: Arc<Names>,
    pub(crate) entities: Arc<Entities>,
    pub(crate) mode: String,
}

pub(crate) fn finish(
    out: fi_kernel::Result<LedgerOutput>,
    errors: &[Arc<Errors>],
    records: &PyRecords,
    entities: &Arc<Entities>,
    book: &Book,
    ws: &Workspace,
) -> PyResult<PyRun> {
    let mode = fi_research::run_mode(ws, &records.reads, book).to_string();
    match out {
        Ok(out) => Ok(PyRun { name: book.name.clone(), out, names: records.names.clone(), entities: entities.clone(), mode }),
        Err(e) => Err(raised(e, &errors.iter().map(AsRef::as_ref).collect::<Vec<_>>())),
    }
}

#[pymethods]
impl PyRun {
    /// What the run is, derived from its decisions and its world (spec, change 4): `"holdings
    /// replay"`, `"policy backtest"`, `"scenario"`, `"counterfactual"` or `"simulation"`.
    #[getter]
    fn mode(&self) -> String {
        self.mode.clone()
    }

    /// The strategy's name.
    #[getter]
    fn name(&self) -> String {
        self.name.clone()
    }

    /// The accounts as a Polars frame, one row per calendar time: NAV, cash, the P&L terms
    /// (income, price, principal, other, financing, currency), costs, capital flows, unsettled
    /// amounts, NAV per unit, turnover, holdings.
    fn ledger<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        convert::to_python(py, fi_io::ledger_frame(&self.out).map_err(convert::err)?)
    }

    /// Every transfer with its cause, as a Polars frame (the book must keep a journal).
    fn journal<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        convert::to_python(py, fi_io::journal_frame(&self.out, &self.entities).map_err(convert::err)?)
    }

    /// Each financing agreement at every time, as a Polars frame: agreement, time, balance,
    /// capacity (a CSA: the netting set's value), call and value (a CSA: collateral required).
    fn financing<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        convert::to_python(py, fi_io::financing_frame(&self.out).map_err(convert::err)?)
    }

    /// Every order's life, as a Polars frame (the book must keep a journal).
    fn orders<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        convert::to_python(py, fi_io::orders_frame(&self.out, &self.entities).map_err(convert::err)?)
    }

    /// The holdings at every time, as a Polars frame (the book must keep them).
    fn holdings<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        convert::to_python(py, fi_io::holdings_frame(&self.out, &self.entities).map_err(convert::err)?)
    }

    /// The orders' totals: submitted, filled, partial, expired, cancelled, requested and filled
    /// value, the delay cost from decision to fill, and what the unfilled part missed.
    #[getter]
    fn order_stats<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let o = &self.out.orders;
        let d = PyDict::new(py);
        d.set_item("submitted", o.submitted)?;
        d.set_item("filled", o.filled)?;
        d.set_item("partial", o.partial)?;
        d.set_item("expired", o.expired)?;
        d.set_item("cancelled", o.cancelled)?;
        d.set_item("requested_value", o.requested_value)?;
        d.set_item("filled_value", o.filled_value)?;
        d.set_item("fill_rate", if o.requested_value > 0.0 { o.filled_value / o.requested_value } else { f64::NAN })?;
        d.set_item("delay_cost", o.delay_cost)?;
        d.set_item("missed", o.missed)?;
        Ok(d)
    }

    /// Each constraint's breaches: count, first time, worst excess, penalty.
    #[getter]
    fn breaches<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyList>> {
        let out = PyList::empty(py);
        for b in &self.out.breaches {
            let d = PyDict::new(py);
            d.set_item("constraint", &b.constraint)?;
            d.set_item("hard", b.hard)?;
            d.set_item("count", b.count)?;
            d.set_item("first", b.first.map(|t| convert::date(py, t)).transpose()?)?;
            d.set_item("worst", b.worst)?;
            d.set_item("penalty", b.penalty)?;
            out.append(d)?;
        }
        Ok(out)
    }

    /// When the book failed economically (an obligation it could not pay, or NAV at or below
    /// zero), or `None`. A failed run stays a result.
    #[getter]
    fn failed_at<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyAny>>> {
        self.out.failed_at.map(|t| convert::date(py, t)).transpose()
    }

    /// The largest obligation the cash could not meet.
    #[getter]
    fn shortfall(&self) -> f64 {
        self.out.shortfall
    }

    /// Purchases cut or refused by the cash guard.
    #[getter]
    fn cash_limited(&self) -> u64 {
        self.out.cash_limited
    }

    /// Sales cut by the short guard.
    #[getter]
    fn short_limited(&self) -> u64 {
        self.out.short_limited
    }

    /// NAV after the horizon liquidation, where the book liquidates.
    #[getter]
    fn liquidation_nav(&self) -> Option<f64> {
        self.out.liquidation_nav
    }

    /// Marked value of what the horizon liquidation could not sell.
    #[getter]
    fn liquidation_residual(&self) -> f64 {
        self.out.liquidation_residual
    }

    /// Cash at the end, by currency.
    #[getter]
    fn final_cash<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        for (c, v) in &self.out.final_cash {
            d.set_item(if *c == EntityId::NONE { "base" } else { self.entities.name(*c) }, v)?;
        }
        Ok(d)
    }

    /// Total return.
    #[getter]
    fn total_return(&self) -> f64 {
        self.out.total_return()
    }

    /// NAV at the end.
    #[getter]
    fn final_nav(&self) -> f64 {
        self.out.nav.last().copied().unwrap_or(f64::NAN)
    }

    /// Trades made.
    #[getter]
    fn trades(&self) -> usize {
        self.out.trades
    }

    /// Trades not made: the unit had no value or no measure at the time.
    #[getter]
    fn skipped_trades(&self) -> u64 {
        self.out.skipped_trades
    }

    /// Orders that filled less than they asked, including not at all (no quote, a size cap, a
    /// book without depth).
    #[getter]
    fn unfilled_orders(&self) -> u64 {
        self.out.unfilled_orders
    }

    /// Decisions still waiting to execute when the calendar ended.
    #[getter]
    fn pending_at_end(&self) -> u64 {
        self.out.pending_at_end
    }

    /// The largest accounting-identity error, as a fraction of NAV.
    #[getter]
    fn max_identity_error(&self) -> f64 {
        self.out.max_identity_error
    }

    /// Holdings at the end: contract name to quantity.
    #[getter]
    fn final_holdings<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let by_id: HashMap<EntityId, &str> = self.names.ids.iter().zip(&self.names.names).map(|(e, n)| (*e, n.as_str())).collect();
        let d = PyDict::new(py);
        for (e, q) in &self.out.final_holdings {
            d.set_item(by_id.get(e).copied().unwrap_or("?"), q)?;
        }
        Ok(d)
    }

    /// Content hash of the accounts.
    #[getter]
    fn version(&self) -> u64 {
        self.out.version()
    }

    /// Statistics from Nautilus's analysis crate, against `benchmark` if given; `period` is
    /// observations per year.
    #[pyo3(signature = (benchmark = None, period = 252))]
    fn report<'py>(&self, py: Python<'py>, benchmark: Option<&Self>, period: usize) -> PyResult<Bound<'py, PyDict>> {
        let r = fi_metrics::report(&self.out, benchmark.map(|b| &b.out), period);
        let d = PyDict::new(py);
        for (k, v) in [
            ("total_return", Some(r.total_return)),
            ("sharpe", Some(r.sharpe)),
            ("sortino", Some(r.sortino)),
            ("volatility", Some(r.volatility)),
            ("max_drawdown", Some(r.max_drawdown)),
            ("cagr", Some(r.cagr)),
            ("tracking_error", r.tracking_error),
            ("information_ratio", r.information_ratio),
            ("up_capture", r.up_capture),
            ("down_capture", r.down_capture),
            ("income", Some(r.income)),
            ("price", Some(r.price)),
            ("principal", Some(r.principal)),
            ("other", Some(r.other)),
            ("financing", Some(r.financing)),
            ("costs", Some(r.costs)),
            ("money_weighted", Some(r.money_weighted)),
        ] {
            d.set_item(k, v)?;
        }
        Ok(d)
    }

    fn __repr__(&self) -> String {
        format!(
            "Run({}, total_return={:.6}, trades={}, identity_error={:.1e})",
            self.name,
            self.out.total_return(),
            self.out.trades,
            self.out.max_identity_error
        )
    }
}
