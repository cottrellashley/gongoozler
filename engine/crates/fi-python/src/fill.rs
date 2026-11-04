//! How orders fill (`fi.Fill`): research, request for quote, auctions, primary allocation, or Nautilus order books.

use super::*;

/// One contract's order book, as given.
pub(crate) struct BookSpec {
    pub(crate) contract: String,
    pub(crate) bids: Vec<Level>,
    pub(crate) asks: Vec<Level>,
    pub(crate) mark: f64,
    pub(crate) per_unit: f64,
}

pub(crate) enum FillSpec {
    Research(f64),
    Rfq(RfqFill),
    Auction(fi_exec::AuctionFill),
    Primary { issues: Vec<(String, f64, f64)>, otherwise: Py<PyFill> },
    Book { books: Vec<BookSpec>, price_precision: u8, size_precision: u8 },
}

/// How orders fill: at research fidelity, by request for quote, or against Nautilus order books.
#[pyclass(name = "Fill", module = "fi_engine._native", frozen)]
pub struct PyFill {
    pub(crate) spec: FillSpec,
}

pub(crate) fn levels(obj: Option<Bound<'_, PyAny>>) -> PyResult<Vec<Level>> {
    let Some(obj) = obj else { return Ok(Vec::new()) };
    let pairs: Vec<(f64, f64)> = obj.extract()?;
    Ok(pairs.into_iter().map(|(price, size)| Level { price, size }).collect())
}

#[pymethods]
impl PyFill {
    /// Every order fills in full at the unit's value plus a half spread (in basis points).
    #[staticmethod]
    #[pyo3(signature = (half_spread_bp = 0.0))]
    pub(crate) fn research(half_spread_bp: f64) -> Self {
        Self { spec: FillSpec::Research(half_spread_bp) }
    }

    /// A bid list (a BWIC): each order is a line auctioned whole to `bidders`, each bidding with
    /// probability `participation`, `discount_bp` to `discount_bp + dispersion_bp` below the
    /// value for a sale (above, for a purchase). The best bid trades the whole line if it is
    /// within `reserve_bp` of the value; otherwise the line does not trade. Deterministic per seed.
    #[staticmethod]
    #[pyo3(signature = (*, bidders = 5, participation = 0.7, discount_bp = 20.0, dispersion_bp = 20.0, reserve_bp = None, seed = 0))]
    fn auction(bidders: usize, participation: f64, discount_bp: f64, dispersion_bp: f64, reserve_bp: Option<f64>, seed: u64) -> Self {
        let reserve_bp = reserve_bp.unwrap_or(f64::INFINITY);
        Self { spec: FillSpec::Auction(fi_exec::AuctionFill { bidders, participation, discount_bp, dispersion_bp, reserve_bp, seed }) }
    }

    /// Primary allocation: an order to buy one of `issues` (`{contract: (reoffer per 100,
    /// allocation)}`) is allocated that fraction of its size at the reoffer price; every other
    /// order fills through `otherwise` (default: research fills at no cost).
    #[staticmethod]
    #[pyo3(signature = (issues, *, otherwise = None))]
    fn primary(py: Python<'_>, issues: HashMap<String, (f64, f64)>, otherwise: Option<Py<PyFill>>) -> PyResult<Self> {
        let mut issues: Vec<(String, f64, f64)> = issues.into_iter().map(|(n, (p, a))| (n, p, a)).collect();
        issues.sort_by(|a, b| a.0.cmp(&b.0));
        if let Some((n, _, a)) = issues.iter().find(|(_, _, a)| !(0.0..=1.0).contains(a)) {
            return Err(PyValueError::new_err(format!("{n}: an allocation of {a}, not a fraction")));
        }
        let otherwise = match otherwise {
            Some(f) => f,
            None => Py::new(py, Self { spec: FillSpec::Research(0.0) })?,
        };
        Ok(Self { spec: FillSpec::Primary { issues, otherwise } })
    }

    /// Request for quote across a dealer panel: dealers respond with probability `response`,
    /// quote around the value, and the best quote fills up to `max_size`. Deterministic per seed.
    #[staticmethod]
    #[pyo3(signature = (*, dealers = 5, response = 0.8, half_spread_bp = 5.0, dispersion_bp = 3.0, max_size = f64::INFINITY, seed = 0))]
    fn rfq(dealers: usize, response: f64, half_spread_bp: f64, dispersion_bp: f64, max_size: f64, seed: u64) -> Self {
        Self { spec: FillSpec::Rfq(RfqFill { dealers, response, half_spread_bp, dispersion_bp, max_size, seed }) }
    }

    /// Fills against Nautilus L2 order books: `books` maps a contract to
    /// `{"bids": [(price, size)], "asks": [...], "mark": m, "per_unit": p}`. A fill's distance
    /// from the mark, times `per_unit`, is its cost per unit.
    #[staticmethod]
    #[pyo3(signature = (books, *, price_precision = 4, size_precision = 0))]
    fn book(books: &Bound<'_, PyDict>, price_precision: u8, size_precision: u8) -> PyResult<Self> {
        let mut out = Vec::new();
        for (k, v) in books.iter() {
            let v = v.cast::<PyDict>().map_err(|_| PyTypeError::new_err("each book is a dict with bids, asks, mark and per_unit"))?;
            let mark: f64 = v.get_item("mark")?.ok_or_else(|| PyValueError::new_err("a book needs a mark"))?.extract()?;
            let per_unit: f64 = v.get_item("per_unit")?.map_or(Ok(1.0), |p| p.extract())?;
            out.push(BookSpec { contract: k.extract()?, bids: levels(v.get_item("bids")?)?, asks: levels(v.get_item("asks")?)?, mark, per_unit });
        }
        Ok(Self { spec: FillSpec::Book { books: out, price_precision, size_precision } })
    }

    fn __repr__(&self) -> String {
        match &self.spec {
            FillSpec::Research(bp) => format!("Fill.research({bp} bp)"),
            FillSpec::Rfq(r) => format!("Fill.{}", r.name()),
            FillSpec::Auction(a) => format!("Fill.{}", a.name()),
            FillSpec::Primary { issues, .. } => format!("Fill.primary({} issues)", issues.len()),
            FillSpec::Book { books, .. } => format!("Fill.book({} books)", books.len()),
        }
    }
}

impl PyFill {
    pub(crate) fn build(&self, names: &Names) -> PyResult<Arc<dyn FillModel>> {
        Ok(match &self.spec {
            FillSpec::Research(bp) => Arc::new(ResearchFill { half_spread_bp: *bp }),
            FillSpec::Rfq(r) => Arc::new(r.clone()),
            FillSpec::Auction(a) => Arc::new(a.clone()),
            FillSpec::Primary { issues, otherwise } => {
                let issues = issues.iter().map(|(n, p, a)| Ok((names.id(n)?, *p, *a))).collect::<PyResult<_>>()?;
                Arc::new(fi_exec::PrimaryFill { issues, otherwise: otherwise.get().build(names)? })
            }
            FillSpec::Book { books, price_precision, size_precision } => {
                let mut f = BookFill::new(*price_precision, *size_precision);
                for b in books {
                    f.set_book(names.id(&b.contract)?, &b.bids, &b.asks, b.mark, b.per_unit);
                }
                Arc::new(f)
            }
        })
    }
}
