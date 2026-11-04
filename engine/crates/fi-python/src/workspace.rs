//! The workspace (`fi.Workspace`): series, entities, models, contracts, records, runs and sweeps, deals and worlds.

use super::*;

/// The workspace: series, entities, and the models' cached outputs.
#[pyclass(name = "Workspace", module = "fi_engine._native", subclass)]
pub struct PyWorkspace {
    pub(crate) ws: Workspace,
}

pub(crate) fn parse_known(k: Option<&Bound<'_, PyAny>>) -> PyResult<Known> {
    let Some(k) = k.filter(|k| !k.is_none()) else { return Ok(Known::Same) };
    if let Ok(c) = k.extract::<String>() {
        return Ok(Known::Column(c));
    }
    if let Ok((what, n)) = k.extract::<(String, i64)>() {
        return match what.as_str() {
            "lag" => Ok(Known::Lag(n)),
            "rows" => usize::try_from(n).map(Known::Rows).map_err(|_| PyValueError::new_err("a lag in rows is not negative")),
            _ => Err(PyValueError::new_err(format!("known is (\"lag\", nanoseconds) or (\"rows\", n), not {what}"))),
        };
    }
    Err(PyTypeError::new_err("known is a column name, fi.lag(...), or None (known at the event time)"))
}

pub(crate) fn parse_unit(u: &Bound<'_, PyAny>) -> PyResult<(Unit, f64, Option<String>)> {
    if let Ok((what, kind)) = u.extract::<(String, String)>() {
        return match what.as_str() {
            "ref" => Ok((Unit::Ref, 1.0, Some(kind))),
            _ => Err(PyValueError::new_err(format!("a unit is a name or (\"ref\", kind), not ({what}, {kind})"))),
        };
    }
    let name: String = u.extract().map_err(|_| PyTypeError::new_err("a unit is a name or fi.ref(kind)"))?;
    Ok(match name.as_str() {
        "number" => (Unit::Number, 1.0, None),
        "price" => (Unit::Price, 1.0, None),
        "rate" => (Unit::Rate, 1.0, None),
        "%" | "percent" => (Unit::Rate, 0.01, None),
        "bp" => (Unit::Rate, 1e-4, None),
        "probability" => (Unit::Probability, 1.0, None),
        "count" => (Unit::Count, 1.0, None),
        "category" => (Unit::Category, 1.0, Some("category".into())),
        "flag" => (Unit::Flag, 1.0, None),
        other => {
            return Err(PyValueError::new_err(format!(
                "unknown unit {other} (number, price, rate, %, bp, probability, count, category, flag, or fi.ref(kind))"
            )));
        }
    })
}

pub(crate) fn parse_missing(m: Option<&Bound<'_, PyAny>>) -> PyResult<MissingPolicy> {
    let Some(m) = m.filter(|m| !m.is_none()) else { return Ok(MissingPolicy::Fail) };
    if let Ok(s) = m.extract::<String>() {
        return match s.as_str() {
            "fail" => Ok(MissingPolicy::Fail),
            "carry" => Ok(MissingPolicy::Carry { max_age: None }),
            _ => Err(PyValueError::new_err(format!("missing is fail, carry, (\"carry\", max_days) or (\"default\", value), not {s}"))),
        };
    }
    let (what, v): (String, Option<f64>) = m.extract()?;
    match (what.as_str(), v) {
        ("carry", d) => Ok(MissingPolicy::Carry { max_age: d.map(days) }),
        ("default", Some(value)) => Ok(MissingPolicy::Default { value }),
        _ => Err(PyValueError::new_err("missing is fail, carry, (\"carry\", max_days) or (\"default\", value)")),
    }
}

impl PyWorkspace {
    pub(crate) fn entity_in(&self, series: &str, name: &str) -> PyResult<EntityId> {
        let s = self.ws.series(series).map_err(to_py)?;
        Names::new(&self.ws, &s.entities()).id(name)
    }
}

#[pymethods]
impl PyWorkspace {
    /// `information` is what decisions and models see of revised values: `"as_known"` (each
    /// vintage from its publication, the default), `"first_release"` (corrections never seen) or
    /// `"legacy_latest"` (revised values dated at first publication: not point in time).
    /// `outcomes` is which vintage contracts step on: `"as_booked"` (the default) or
    /// `"restated"`.
    #[new]
    #[pyo3(signature = (information = "as_known", outcomes = "as_booked"))]
    fn new(information: &str, outcomes: &str) -> PyResult<Self> {
        let information = match information {
            "as_known" => Information::AsKnown,
            "first_release" => Information::FirstRelease,
            "legacy_latest" => Information::LegacyLatest,
            i => return Err(PyValueError::new_err(format!("information is as_known, first_release or legacy_latest, not {i}"))),
        };
        let outcomes = match outcomes {
            "as_booked" => Outcomes::AsBooked,
            "restated" => Outcomes::Restated,
            o => return Err(PyValueError::new_err(format!("outcomes is as_booked or restated, not {o}"))),
        };
        Ok(Self { ws: Workspace::new(KnowledgePolicy { information, outcomes }) })
    }

    /// What decisions and models see of revised values.
    #[getter]
    fn information(&self) -> &'static str {
        match self.ws.knowledge().information {
            Information::AsKnown => "as_known",
            Information::FirstRelease => "first_release",
            Information::LegacyLatest => "legacy_latest",
        }
    }

    /// Which vintage defines outcomes.
    #[getter]
    fn outcomes(&self) -> &'static str {
        match self.ws.knowledge().outcomes {
            Outcomes::AsBooked => "as_booked",
            Outcomes::Restated => "restated",
        }
    }

    /// Where a series comes from: how its knowledge times were established, who produces it, and
    /// the state it describes.
    fn provenance<'py>(&self, py: Python<'py>, series: &str) -> PyResult<Bound<'py, PyDict>> {
        let s = self.ws.series(series).map_err(to_py)?;
        let p = s.provenance();
        let d = PyDict::new(py);
        match &p.knowledge {
            KnowledgeBasis::Observed => d.set_item("knowledge", "observed")?,
            KnowledgeBasis::Assumed(how) => {
                d.set_item("knowledge", "assumed")?;
                d.set_item("assumption", how)?;
            }
            KnowledgeBasis::Derived => d.set_item("knowledge", "derived")?,
        }
        match &p.producer {
            Producer::Observed => d.set_item("producer", "observation")?,
            Producer::Model { name, role } => {
                d.set_item("producer", format!("model {name}"))?;
                d.set_item("role", role_name(*role))?;
            }
            Producer::Book { name } => d.set_item("producer", format!("book {name}"))?,
        }
        d.set_item("describes", p.describes.clone())?;
        d.set_item("intervention", p.intervention.as_ref().map(|i| (i.kind(), i.by().to_owned())))?;
        d.set_item("revisions", s.has_revisions())?;
        Ok(d)
    }

    /// Registers a frame as a series, checked against its declaration. `data` is anything with
    /// `__arrow_c_stream__`; only the declared columns are read. Returns a summary.
    #[pyo3(signature = (name, data, *, entity = None, kind = None, time = None, known = None, values = None, grid = false, nan_is_missing = false, describes = None, scenario = None))]
    #[allow(clippy::too_many_arguments)]
    fn series<'py>(
        &mut self,
        py: Python<'py>,
        name: &str,
        data: PyTable,
        entity: Option<String>,
        kind: Option<String>,
        time: Option<String>,
        known: Option<&Bound<'py, PyAny>>,
        values: Option<&Bound<'py, PyDict>>,
        grid: bool,
        nan_is_missing: bool,
        describes: Option<String>,
        scenario: Option<String>,
    ) -> PyResult<Bound<'py, PyDict>> {
        let mut decl = SeriesDecl::new(name).known(parse_known(known)?);
        decl.scenario = scenario;
        if let Some(d) = &describes {
            decl = decl.describes(d);
        }
        let mut cols: Vec<String> = Vec::new();
        if let Some(e) = &entity {
            decl = decl.entity(e, kind.as_deref().unwrap_or(e));
            cols.push(e.clone());
        }
        if let Some(t) = &time {
            decl = decl.time(t);
            cols.push(t.clone());
        }
        if let Known::Column(c) = &decl.known {
            cols.push(c.clone());
        }
        for (k, u) in values.into_iter().flat_map(|v| v.iter()) {
            let column: String = k.extract()?;
            let (unit, scale, kind) = parse_unit(&u)?;
            cols.push(column.clone());
            decl.values.push(ValueSpec { column, unit, scale, kind });
        }
        if grid {
            decl = decl.grid();
        }
        if nan_is_missing {
            decl = decl.nan_is_missing();
        }
        let mut seen = HashSet::new();
        cols.retain(|c| seen.insert(c.clone()));
        let df = convert::frame(data, &cols.iter().map(String::as_str).collect::<Vec<_>>())?;
        let ws = &mut self.ws;
        let s = py.detach(|| fi_io::register(ws, &df, &decl)).map_err(to_py)?;
        let d = PyDict::new(py);
        d.set_item("name", name)?;
        d.set_item("rows", df.height())?;
        d.set_item("values", s.len())?;
        d.set_item("entities", s.entities().len())?;
        d.set_item("layout", if matches!(s.layout(), Layout::Grid { .. }) { "grid" } else { "rows" })?;
        d.set_item("version", s.version())?;
        Ok(d)
    }

    /// Series names.
    fn names(&self) -> Vec<String> {
        self.ws.names()
    }

    /// The entities a series describes.
    fn entities(&self, series: &str) -> PyResult<Vec<String>> {
        let s = self.ws.series(series).map_err(to_py)?;
        Ok(s.entities().iter().map(|e| self.ws.entities.name(*e).to_owned()).collect())
    }

    /// A series (registered or a model's output) as a long Polars frame.
    fn frame<'py>(&self, py: Python<'py>, series: &str) -> PyResult<Bound<'py, PyAny>> {
        let s = self.ws.series(series).map_err(to_py)?;
        convert::to_python(py, fi_io::series_frame(s, &self.ws.entities).map_err(convert::err)?)
    }

    /// A series' own event times, as a calendar (the data's own calendar).
    fn calendar(&self, series: &str) -> PyResult<PyCalendar> {
        let s = self.ws.series(series).map_err(to_py)?;
        let times = match s.layout() {
            Layout::Grid { times, .. } => times.clone(),
            Layout::Rows { event, .. } => event.iter().copied().filter(|t| *t != Ts::MIN).collect(),
        };
        Ok(PyCalendar { cal: Calendar::new(times) })
    }

    /// Evaluates models over `calendar`, in dependency order, in parallel where they allow it.
    /// Outputs are cached by their inputs' versions: a second evaluation is free.
    /// `substitute` maps a state (a series a model writes) to a registered observation of it,
    /// to replay history: refused where anything upstream of the state is intervened. The
    /// report names what was evaluated, cached, substituted and intervened, and the mode.
    #[pyo3(signature = (models, calendar, *, substitute = None))]
    fn evaluate<'py>(
        &mut self,
        py: Python<'py>,
        models: Vec<PyRef<'py, PyModel>>,
        calendar: &PyCalendar,
        substitute: Option<HashMap<String, String>>,
    ) -> PyResult<Bound<'py, PyDict>> {
        let names = Arc::new((0..self.ws.entities.len()).map(|i| self.ws.entities.name(EntityId(i as u32)).to_owned()).collect::<Vec<_>>());
        let mut g = Graph::new();
        let mut pythons = Vec::new();
        for m in &models {
            if let Some(p) = &m.python {
                *p.names.write().expect("not poisoned") = names.clone();
                pythons.push(p.clone());
            }
            g.add(m.model.clone());
        }
        for (state, obs) in substitute.iter().flatten() {
            g.substitute(state, obs);
        }
        let (ws, cal) = (&mut self.ws, &calendar.cal);
        let report = py.detach(|| g.evaluate(ws, cal)).map_err(|e| {
            let mut errors: Vec<&Errors> = pythons.iter().map(|p| &p.errors).collect();
            errors.extend(models.iter().flat_map(|m| m.errors.iter().map(AsRef::as_ref)));
            raised(e, &errors)
        })?;
        let d = PyDict::new(py);
        d.set_item("mode", report.mode().to_string())?;
        d.set_item("substituted", report.substituted.iter().cloned().collect::<HashMap<String, String>>())?;
        let intervened = PyDict::new(py);
        for (m, i) in &report.intervened {
            intervened.set_item(m, (i.kind(), i.by()))?;
        }
        d.set_item("intervened", intervened)?;
        d.set_item("evaluated", report.evaluated)?;
        d.set_item("cached", report.cached)?;
        Ok(d)
    }

    /// Bonds (or any unit) priced from supplied marks: one contract per entity of `marks`
    /// (or the named ones). One unit is one unit of original face; marks are per 100 of current
    /// face; `flows` (if given) holds `interest`, `principal_face`, `principal_price`,
    /// `writedown_face`, `writeup_face` and `pik_face`. A seasoned bond names its `factor` column
    /// in the marks, read at the first time.
    /// `currency` names the bonds' currency (default: the book's base). With `reconcile`, a coupon
    /// the accrued interest does not fall back from is refused, since it would count twice; turn
    /// it off only for steps as long as a coupon period.
    #[pyo3(signature = (marks, *, clean = "clean", accrued = "accrued", flows = None, factor = None, entities = None, currency = None, reconcile = true))]
    #[allow(clippy::too_many_arguments)]
    fn supplied_bonds(
        &mut self,
        marks: &str,
        clean: &str,
        accrued: &str,
        flows: Option<String>,
        factor: Option<String>,
        entities: Option<Vec<String>>,
        currency: Option<&str>,
        reconcile: bool,
    ) -> PyResult<PyContracts> {
        let ccy = currency.map(|c| self.ws.entities.intern("currency", c));
        let s = self.ws.series(marks).map_err(to_py)?;
        s.column(clean).map_err(to_py)?;
        s.column(accrued).map_err(to_py)?;
        if let Some(f) = &factor {
            s.column(f).map_err(to_py)?;
        }
        if let Some(f) = &flows {
            self.ws.series(f).map_err(to_py)?;
        }
        let ids: Vec<EntityId> = match entities {
            None => s.entities(),
            Some(n) => {
                let own = Names::new(&self.ws, &s.entities());
                n.iter().map(|x| own.id(x)).collect::<PyResult<_>>()?
            }
        };
        let inner = ids
            .into_iter()
            .map(|e| {
                let bond = SuppliedBond {
                    entity: e,
                    marks: marks.into(),
                    clean: clean.into(),
                    accrued: accrued.into(),
                    flows: flows.clone(),
                    factor: factor.clone(),
                    reconcile,
                };
                in_currency(Arc::new(bond), ccy)
            })
            .collect();
        Ok(PyContracts { inner })
    }

    /// Debt contracts from terms: `bonds` maps each instrument's name to its `Bond`. `valuation`
    /// says where clean prices come from (`fi.valuation.clean`, `.yield_` or `.curve`).
    /// `fixings` (`series.column`) is where floating and inflation coupons fix, keyed by an entity
    /// named as the coupon's index. `events` names a series of observed lifecycle events keyed by
    /// instrument, with any of the columns `call` and `put` (price per 100), `default` (1),
    /// `recovery` (cash per 100 of face), `writedown` and `writeup` (face per unit of original
    /// face), `factor` (face outstanding, as reported) and `extend` (1). The instruments are
    /// entities of `kind`; each is held in its `Bond`'s currency (default: the book's base).
    #[pyo3(signature = (bonds, valuation, *, fixings = None, events = None, kind = "bond"))]
    fn debt(
        &mut self,
        bonds: &Bound<'_, PyDict>,
        valuation: &Bound<'_, PyAny>,
        fixings: Option<&str>,
        events: Option<&str>,
        kind: &str,
    ) -> PyResult<PyContracts> {
        Ok(PyContracts { inner: terms::debt_contracts(&mut self.ws, bonds, valuation, fixings, events, kind)? })
    }

    /// A credit default swap, single-name or index: one unit is one unit of notional of protection
    /// bought (a seller holds a negative quantity). The buyer pays `coupon` a year on the notional
    /// outstanding, on a schedule from `start` to `maturity` (`frequency` a year, `day_count`,
    /// adjusted on `calendar`), and is paid one less the recovery on each reference entity's credit
    /// event. `names` is the reference entity (default: `name`), or an index's `{name: weight}`; a
    /// name's event reduces the index's notional by its weight. `price` (`series.column`, keyed by
    /// `name`) is quoted per 100 of current notional from the seller's side (100 less the upfront);
    /// `events` names a series keyed by reference entity with `default` (1) and `recovery` (per 100).
    #[pyo3(signature = (name, price, *, coupon, start, maturity, names = None, frequency = 4, day_count = "ACT/360", calendar = None, business_day = "following", events = None))]
    #[allow(clippy::too_many_arguments)]
    fn cds(
        &mut self,
        name: &str,
        price: &str,
        coupon: f64,
        start: &Bound<'_, PyAny>,
        maturity: &Bound<'_, PyAny>,
        names: Option<&Bound<'_, PyAny>>,
        frequency: u32,
        day_count: &str,
        calendar: Option<PyRef<'_, terms::PyBusinessCalendar>>,
        business_day: &str,
        events: Option<&str>,
    ) -> PyResult<PyContracts> {
        let inner = terms::cds_contract(&mut self.ws, name, price, coupon, start, maturity, names, frequency, day_count, calendar, business_day, events)?;
        Ok(PyContracts { inner })
    }

    /// An interest rate swap: the `receive` leg less the `pay` leg (each a `Bond`, on one unit of
    /// notional, their principal exchanges cancelling), both valued as `valuation` says (a curve,
    /// typically) with a floater's unfixed coupons at its forwards and its cash from `fixings`.
    /// Unfunded: paying the received leg (a negative quantity) needs no borrowed security.
    #[pyo3(signature = (name, receive, pay, valuation, *, fixings = None, kind = "swap"))]
    fn swap(
        &mut self,
        name: &str,
        receive: &Bound<'_, PyAny>,
        pay: &Bound<'_, PyAny>,
        valuation: &Bound<'_, PyAny>,
        fixings: Option<&str>,
        kind: &str,
    ) -> PyResult<PyContracts> {
        Ok(PyContracts { inner: terms::swap_contract(&mut self.ws, name, receive, pay, valuation, fixings, kind)? })
    }

    /// A tradable level (an index, a total-return series): one unit is worth the level, in
    /// `currency` (default: the book's base).
    #[pyo3(signature = (entity, level, *, currency = None))]
    fn total_return(&mut self, entity: &str, level: &str, currency: Option<&str>) -> PyResult<PyContracts> {
        let (series, column) = split(level)?;
        self.ws.series(series).and_then(|s| s.column(column)).map_err(to_py)?;
        let e = self.entity_in(series, entity)?;
        let ccy = currency.map(|c| self.ws.entities.intern("currency", c));
        Ok(PyContracts { inner: vec![in_currency(Arc::new(TotalReturn { entity: e, series: series.into(), column: column.into() }), ccy)] })
    }

    /// A cash account, accruing `rate` (`series.column`, a market-wide rate) in `day_count` if
    /// given.
    #[pyo3(signature = (name = "cash", rate = None, *, day_count = "ACT/360"))]
    fn cash_account(&mut self, name: &str, rate: Option<&str>, day_count: &str) -> PyResult<PyContracts> {
        let day_count: fi_kernel::DayCount = day_count.parse().map_err(|e: fi_terms::Error| PyValueError::new_err(e.to_string()))?;
        let rate = rate.map(split).transpose()?.map(|(s, c)| (s.to_owned(), c.to_owned()));
        if let Some((s, c)) = &rate {
            self.ws.series(s).and_then(|x| x.column(c)).map_err(to_py)?;
        }
        let entity = self.ws.entities.intern("cash", name);
        Ok(PyContracts { inner: vec![Arc::new(CashAccount { entity, rate, day_count })] })
    }

    /// A future, marked to market daily: one unit gains `multiplier` × the price change, paid as
    /// variation margin in `currency` (default: the book's base).
    #[pyo3(signature = (entity, price, multiplier, *, currency = None))]
    fn daily_future(&mut self, entity: &str, price: &str, multiplier: f64, currency: Option<&str>) -> PyResult<PyContracts> {
        let (series, column) = split(price)?;
        self.ws.series(series).and_then(|s| s.column(column)).map_err(to_py)?;
        let e = self.entity_in(series, entity)?;
        let ccy = currency.map(|c| self.ws.entities.intern("currency", c));
        Ok(PyContracts { inner: vec![in_currency(Arc::new(DailyFuture { entity: e, series: series.into(), column: column.into(), multiplier }), ccy)] })
    }

    /// An exchange-traded future, its terms a Nautilus `FuturesContract` built from these fields
    /// and checked by Nautilus: `symbol` on `venue`, of `underlying`, listed at `activation` and
    /// expiring at `expiration`, in `currency`, with its tick (`price_increment`, whose decimals
    /// are its precision), `multiplier` and `lot_size`. One unit is one contract, marked to market
    /// each step at `price` (`series.column`, keyed by `name`), so its P&L is variation margin.
    #[pyo3(signature = (name, price, *, expiration, multiplier, price_increment, activation = None, underlying = None, venue = "XCME", currency = "USD", lot_size = 1.0, asset_class = "debt"))]
    #[allow(clippy::too_many_arguments)]
    fn future(
        &mut self,
        name: &str,
        price: &str,
        expiration: &Bound<'_, PyAny>,
        multiplier: f64,
        price_increment: &str,
        activation: Option<&Bound<'_, PyAny>>,
        underlying: Option<&str>,
        venue: &str,
        currency: &str,
        lot_size: f64,
        asset_class: &str,
    ) -> PyResult<PyContracts> {
        let (series, column) = split(price)?;
        self.ws.series(series).and_then(|s| s.column(column)).map_err(to_py)?;
        let e = self.entity_in(series, name)?;
        let instrument = book::futures_contract(
            name,
            venue,
            underlying.unwrap_or(name),
            activation,
            expiration,
            currency,
            price_increment,
            multiplier,
            lot_size,
            asset_class,
        )?;
        let ccy = Some(self.ws.entities.intern("currency", currency));
        let c = fi_exec::ExchangeFuture { entity: e, instrument, price: (series.into(), column.into()), currency: ccy };
        Ok(PyContracts { inner: vec![Arc::new(c)] })
    }

    /// The run's manifest: everything its output depends on besides the data's values (series
    /// versions and provenance, the knowledge policy, models with their fingerprints, roles and
    /// vintages, the calendar, the book's policies and settings, the mode, approximations), and
    /// the output's version (spec, change 7).
    fn manifest(&mut self, py: Python<'_>, run: &PyRun, book: &PyBook, records: &PyRecords) -> PyResult<PyManifest> {
        let (b, _) = book.build(py, &self.ws, records)?;
        Ok(PyManifest { m: fi_research::Manifest::of(&self.ws, &records.rec, &records.reads, &b, &run.out, &records.approximations) })
    }

    /// A run's NAV against an official one (`"series.column"`, for `entity` or market-wide):
    /// date, engine, official and difference. Nothing is plugged: what the engine does not
    /// explain stays visible.
    #[pyo3(signature = (run, official, *, entity = None))]
    fn reconcile<'py>(&self, py: Python<'py>, run: &PyRun, official: &str, entity: Option<&str>) -> PyResult<Bound<'py, PyAny>> {
        let (s, c) = split(official)?;
        let e = match entity {
            Some(n) => self.entity_in(s, n)?,
            None => EntityId::NONE,
        };
        let r = fi_research::reconcile(&self.ws, &run.out, s, c, e).map_err(to_py)?;
        let d = PyDict::new(py);
        d.set_item("date", r.times.iter().map(|t| convert::date(py, *t)).collect::<PyResult<Vec<_>>>()?)?;
        d.set_item("engine", r.engine)?;
        d.set_item("official", r.official.iter().map(|x| x.is_finite().then_some(*x)).collect::<Vec<_>>())?;
        d.set_item("difference", r.difference.iter().map(|x| x.is_finite().then_some(*x)).collect::<Vec<_>>())?;
        py.import("polars")?.call_method1("DataFrame", (d,))
    }

    /// A world: a copy of this workspace that shares every series and cached output with it by
    /// reference (spec, change 6). Evaluate what does not vary by world here first; then each
    /// world evaluates its own stochastic models.
    fn world(&self) -> Self {
        Self { ws: self.ws.clone() }
    }

    /// Makes `other` a world of this workspace (for subclasses: `fi.Workspace.world`).
    fn _share_into(&self, mut other: PyRefMut<'_, Self>) {
        other.ws = self.ws.clone();
    }

    /// Correlated Gaussian paths, a real-world model writing series `name` (column `value`):
    /// an Ornstein–Uhlenbeck process for each entity of `start` (`{name: first value}`, of
    /// `kind`; a number for one market-wide series), reverting at `reversion` per year to
    /// `mean` (or drifting at `drift` without reversion), with volatility `vol` per square-root
    /// year and shocks loading `correlation` on a common factor. Draws are keyed by `seed`,
    /// `world`, entity and time.
    #[pyo3(signature = (name, start, *, kind = "index", mean = 0.0, reversion = 0.0, drift = 0.0, vol = 0.01, correlation = 0.0, floor = None, seed = 0, world = 0))]
    #[allow(clippy::too_many_arguments)]
    fn gaussian_paths(
        &mut self,
        name: &str,
        start: &Bound<'_, PyAny>,
        kind: &str,
        mean: f64,
        reversion: f64,
        drift: f64,
        vol: f64,
        correlation: f64,
        floor: Option<f64>,
        seed: u64,
        world: u64,
    ) -> PyResult<PyModel> {
        let start = if let Ok(x) = start.extract::<f64>() {
            vec![(EntityId::NONE, x)]
        } else {
            let d = start.cast::<PyDict>().map_err(|_| PyTypeError::new_err("start is a number (market-wide) or {name: first value}"))?;
            d.iter().map(|(k, v)| Ok((self.ws.entities.intern(kind, &k.extract::<String>()?), v.extract::<f64>()?))).collect::<PyResult<_>>()?
        };
        let m = GaussianPaths { name: name.into(), start, mean, reversion, drift, vol, correlation, floor, seed, world };
        Ok(PyModel { model: fi_kernel::Model::batch(m), python: None, errors: Vec::new() })
    }

    /// Loan cash flows in a world with correlated defaults, a real-world model writing the rows
    /// series `name` (`interest`, `principal_face`, `writedown_face`, `defaulted`, per unit of
    /// face) for `supplied_bonds` loans. `loans` lists `(loan, borrower, coupon, maturity,
    /// hazard, recovery)`: one borrower's loans default together, each recovering its own share.
    /// Defaults draw exponential times through a one-factor Gaussian copula with `correlation`,
    /// recognised on the next of `payments`.
    #[pyo3(signature = (name, loans, payments, *, correlation = 0.0, seed = 0, world = 0))]
    fn defaults(
        &mut self,
        name: &str,
        loans: Vec<(String, String, f64, Bound<'_, PyAny>, f64, f64)>,
        payments: Vec<Bound<'_, PyAny>>,
        correlation: f64,
        seed: u64,
        world: u64,
    ) -> PyResult<PyModel> {
        let loans = loans
            .into_iter()
            .map(|(l, b, coupon, maturity, hazard, recovery)| {
                Ok(LoanTerms {
                    entity: self.ws.entities.intern("loan", &l),
                    borrower: self.ws.entities.intern("borrower", &b),
                    coupon,
                    maturity: convert::ts(&maturity)?,
                    hazard,
                    recovery,
                })
            })
            .collect::<PyResult<_>>()?;
        let payments = payments.iter().map(convert::ts).collect::<PyResult<_>>()?;
        let m = Defaults { name: name.into(), loans, payments, correlation, seed, world };
        Ok(PyModel { model: fi_kernel::Model::batch(m), python: None, errors: Vec::new() })
    }

    /// A deal (a securitisation): a collateral book and its priority of payments, as a model.
    /// Evaluating it on `calendar` writes `{name}.flows`, `.marks`, `.distributions`, `.tests`,
    /// `.accounts`, `.collateral` and `.claims`; its tranches are then contracts
    /// (`ws.tranches(name)`). The terms come from `fi_engine.deals`; steps are strings such as
    /// `"interest:A"` or `"sequential:A,B when amortising"`. Entities are named `{name}.{part}`.
    ///
    /// `rounding` is the indenture's: `(unit or currency, mode)`, such as `("USD", "half_up")`.
    /// Every amount due and paid is then whole minor units, a pro rata split gives each odd unit
    /// to the largest remainder, and a cure is the smallest whole amount that cures. Default: none.
    #[pyo3(signature = (
        name, calendar, *, collateral, opening, tranches, interest, principal = Vec::new(), payments = None, fees = None, tests = None, haircuts = None,
        triggers = None, losses = None, cash = 0.0, reserve = 0.0, reserve_target = 0.0, reinvest_until = None, maturity = None, rights = None,
        exercises = None, manager = None, fill = None, missing = None, rounding = None, reads = Vec::new()
    ))]
    #[allow(clippy::too_many_arguments)]
    fn deal<'py>(
        &mut self,
        py: Python<'py>,
        name: &str,
        calendar: &PyCalendar,
        collateral: &PyContracts,
        opening: Bound<'py, PyDict>,
        tranches: Bound<'py, PyAny>,
        interest: Vec<String>,
        principal: Vec<String>,
        payments: Option<Bound<'py, PyAny>>,
        fees: Option<Bound<'py, PyAny>>,
        tests: Option<Bound<'py, PyAny>>,
        haircuts: Option<Bound<'py, PyAny>>,
        triggers: Option<Bound<'py, PyAny>>,
        losses: Option<Bound<'py, PyAny>>,
        cash: f64,
        reserve: f64,
        reserve_target: f64,
        reinvest_until: Option<Bound<'py, PyAny>>,
        maturity: Option<Bound<'py, PyAny>>,
        rights: Option<Bound<'py, PyAny>>,
        exercises: Option<Bound<'py, PyAny>>,
        manager: Option<Bound<'py, PyAny>>,
        fill: Option<PyRef<'py, PyFill>>,
        missing: Option<Bound<'py, PyAny>>,
        rounding: Option<Bound<'py, PyAny>>,
        reads: Vec<String>,
    ) -> PyResult<PyModel> {
        let args = deal::DealArgs {
            name: name.to_owned(),
            calendar: calendar.cal.clone(),
            collateral: collateral.inner.clone(),
            opening,
            tranches,
            payments,
            interest,
            principal,
            fees,
            tests,
            haircuts,
            triggers,
            losses,
            cash,
            reserve,
            reserve_target,
            reinvest_until,
            maturity,
            rights,
            exercises,
            manager,
            fill,
            missing: parse_missing(missing.as_ref())?,
            rounding: book::parse_rounding(rounding.as_ref())?,
            reads,
        };
        let (d, errors) = deal::build(py, &mut self.ws, args)?;
        Ok(PyModel { model: fi_kernel::Model::batch(d), python: None, errors })
    }

    /// Registers a currency (an entity of kind `currency`), so it can be a book's base.
    fn currency(&mut self, name: &str) {
        self.ws.entities.intern("currency", name);
    }

    /// Steps `contracts` over `calendar` into unit records. `missing` is `"fail"` (default),
    /// `"carry"`, `("carry", max_days)` or `("default", value)`; whatever is carried or
    /// defaulted is counted. `requires` maps a contract kind to the features its contracts must
    /// model (`{"supplied_bond": ["writedown"]}`); a gap is refused unless `approximate` declares
    /// it, and declared approximations go into the run's manifest. `transitions`
    /// (`fi.transition(...)`) move every holder of a contract into others and cash at an event: an
    /// exchange, a conversion, the settlement of a claim.
    #[pyo3(signature = (contracts, calendar, missing = None, *, requires = None, approximate = None, transitions = None))]
    #[allow(clippy::too_many_arguments)]
    fn records(
        &self,
        py: Python<'_>,
        contracts: &PyContracts,
        calendar: &PyCalendar,
        missing: Option<&Bound<'_, PyAny>>,
        requires: Option<HashMap<String, Vec<String>>>,
        approximate: Option<HashMap<String, Vec<String>>>,
        transitions: Option<Vec<Bound<'_, PyAny>>>,
    ) -> PyResult<PyRecords> {
        let policy = parse_missing(missing)?;
        let pairs = |m: Option<HashMap<String, Vec<String>>>| -> Vec<(String, String)> {
            let mut v: Vec<(String, String)> = m.into_iter().flatten().flat_map(|(k, fs)| fs.into_iter().map(move |f| (k.clone(), f))).collect();
            v.sort();
            v
        };
        let approximations = fi_research::check_requirements(&self.ws, &contracts.inner, &pairs(requires), &pairs(approximate)).map_err(to_py)?;
        let (ws, c, cal) = (&self.ws, &contracts.inner, &calendar.cal);
        let (mut rec, d) = py.detach(|| UnitRecords::compute(c, ws, cal, policy)).map_err(to_py)?;
        let names = Arc::new(Names::new(&self.ws, rec.entities()));
        if let Some(list) = transitions {
            let parsed = list.iter().map(|x| transition(x, &names)).collect::<PyResult<Vec<_>>>()?;
            rec.with_transitions(parsed, &self.ws.entities).map_err(to_py)?;
        }
        let mut reads: Vec<String> = c.iter().flat_map(|x| x.reads()).collect();
        reads.sort();
        reads.dedup();
        Ok(PyRecords { rec: Arc::new(rec), names, diagnostics: d.snapshot(), reads: Arc::new(reads), approximations: Arc::new(approximations) })
    }

    /// Each contract's P&L per unit explained at each time (review M7): `total`, and where the
    /// contract explains itself (a debt valued off a curve), `carry` (the accrual, the flows and
    /// the roll down the curve, with the market unchanged in tenor), `curve` and `spread`, which
    /// add up to it; `residual` is what they leave (all of it where nothing explains it). Steps
    /// the contracts again; nothing is kept in the records. `fi.attribution` weights it by a run's
    /// holdings.
    #[pyo3(signature = (contracts, calendar, missing = None))]
    fn explain<'py>(&self, py: Python<'py>, contracts: &PyContracts, calendar: &PyCalendar, missing: Option<&Bound<'_, PyAny>>) -> PyResult<Bound<'py, PyAny>> {
        let policy = parse_missing(missing)?;
        let (ws, c, cal) = (&self.ws, &contracts.inner, &calendar.cal);
        let x = py.detach(|| fi_kernel::Explained::compute(c, ws, cal, policy)).map_err(to_py)?;
        convert::to_python(py, fi_io::explained_frame(&x, cal, &self.ws.entities).map_err(convert::err)?)
    }

    /// The records plus a composite: a portfolio of `legs` held as one contract. `schedule` lists
    /// `(time, value weights)`; the first must be at or before the calendar's first time; later
    /// entries are rolls, each costing `roll_cost_bp` of the value traded.
    #[pyo3(signature = (records, name, legs, schedule, roll_cost_bp = 0.0))]
    fn composite(
        &mut self,
        records: &PyRecords,
        name: &str,
        legs: Vec<String>,
        schedule: Vec<(Bound<'_, PyAny>, Vec<f64>)>,
        roll_cost_bp: f64,
    ) -> PyResult<PyRecords> {
        let legs = legs.iter().map(|l| records.names.id(l)).collect::<PyResult<_>>()?;
        let schedule = schedule.iter().map(|(t, w)| Ok((convert::ts(t)?, w.clone()))).collect::<PyResult<_>>()?;
        let entity = self.ws.entities.intern("composite", name);
        let (built, _) = Composite { entity, legs, schedule, roll_cost_bp }.build(&records.rec).map_err(to_py)?;
        let mut rec = (*records.rec).clone();
        rec.extend(&built).map_err(to_py)?;
        let names = Arc::new(Names::new(&self.ws, rec.entities()));
        Ok(PyRecords {
            rec: Arc::new(rec),
            names,
            diagnostics: records.diagnostics,
            reads: records.reads.clone(),
            approximations: records.approximations.clone(),
        })
    }

    /// Runs a book (or a strategy) on the records.
    fn run(&mut self, py: Python<'_>, book: &PyBook, records: &PyRecords) -> PyResult<PyRun> {
        if let Some(c) = book.currency() {
            self.ws.entities.intern("currency", c);
        }
        let entities = Arc::new(self.ws.entities.clone());
        let (b, errors) = book.build(py, &self.ws, records)?;
        let (ws, rec) = (&self.ws, records.rec.as_ref());
        let out = py.detach(|| fi_research::run_book(ws, rec, &b));
        finish(out, &errors, records, &entities, &b, &self.ws)
    }

    /// Runs many books in parallel on the same records; results in input order.
    fn sweep(&mut self, py: Python<'_>, books: Vec<PyRef<'_, PyBook>>, records: &PyRecords) -> PyResult<Vec<PyRun>> {
        for b in &books {
            if let Some(c) = b.currency() {
                self.ws.entities.intern("currency", c);
            }
        }
        let entities = Arc::new(self.ws.entities.clone());
        let built: Vec<(Book, Vec<Arc<Errors>>)> = books.iter().map(|b| b.build(py, &self.ws, records)).collect::<PyResult<_>>()?;
        let list: Vec<Book> = built.iter().map(|(b, _)| b.clone()).collect();
        let (ws, rec) = (&self.ws, records.rec.as_ref());
        let outs = py.detach(|| fi_research::sweep_books(ws, rec, &list));
        outs.into_iter().zip(&built).map(|(o, (b, e))| finish(o, e, records, &entities, b, &self.ws)).collect()
    }

    /// Runs several books in lockstep through one shared `execution`, which sees every book's
    /// orders together: the books compete for its capacity (accounts that must be implementable
    /// at the same time). Each book's own execution is ignored.
    fn run_together(&mut self, py: Python<'_>, books: Vec<PyRef<'_, PyBook>>, records: &PyRecords, execution: &PyExecution) -> PyResult<Vec<PyRun>> {
        for b in &books {
            if let Some(c) = b.currency() {
                self.ws.entities.intern("currency", c);
            }
        }
        let entities = Arc::new(self.ws.entities.clone());
        let built: Vec<(Book, Vec<Arc<Errors>>)> = books.iter().map(|b| b.build(py, &self.ws, records)).collect::<PyResult<_>>()?;
        let list: Vec<Book> = built.iter().map(|(b, _)| b.clone()).collect();
        let shared = execution.build(&records.names)?;
        let errors: Vec<Arc<Errors>> = built.iter().flat_map(|(_, e)| e.iter().cloned()).collect();
        let (ws, rec) = (&self.ws, records.rec.as_ref());
        let outs = py
            .detach(|| fi_research::run_together(ws, rec, &list, shared.as_ref()))
            .map_err(|e| raised(e, &errors.iter().map(AsRef::as_ref).collect::<Vec<_>>()))?;
        outs.into_iter().zip(&built).map(|(o, (b, e))| finish(Ok(o), e, records, &entities, b, &self.ws)).collect()
    }

    /// A synthetic universe (the one the Rust tests and speed tests use): bonds with marks,
    /// accrued and flows, issuers with equity and filed financials, governments, an index, a rate.
    #[pyo3(signature = (*, bonds = 200, days = 520, seed = 7, issuers = 20, govts = 30, amortising_every = 5, pik_every = 11, gap_rate = 0.0))]
    #[allow(clippy::too_many_arguments)]
    fn synthetic<'py>(
        &mut self,
        py: Python<'py>,
        bonds: usize,
        days: usize,
        seed: u64,
        issuers: usize,
        govts: usize,
        amortising_every: usize,
        pik_every: usize,
        gap_rate: f64,
    ) -> PyResult<Bound<'py, PyDict>> {
        let cfg = synth::Config { bonds, days, seed, amortising_every, pik_every, gap_rate, issuers, govts };
        let ws = &mut self.ws;
        let u = py.detach(|| synth::build(ws, &cfg)).map_err(to_py)?;
        let d = PyDict::new(py);
        d.set_item("calendar", Bound::new(py, PyCalendar { cal: u.calendar.clone() })?)?;
        d.set_item("bonds", Bound::new(py, PyContracts { inner: u.bond_contracts() })?)?;
        d.set_item("index", Bound::new(py, PyContracts { inner: vec![u.index_contract()] })?)?;
        d.set_item("gaps", u.gaps)?;
        Ok(d)
    }

    fn __repr__(&self) -> String {
        format!(
            "Workspace({} series, {} entities, information={}, outcomes={})",
            self.ws.names().len(),
            self.ws.entities.len(),
            self.information(),
            self.outcomes()
        )
    }
}

/// This machine against the one the speed budgets were set on: the factors (single-threaded,
/// parallel) a budget scales by here, never below one (`fi_research::machine`).
#[pyfunction]
pub(crate) fn machine_factors(py: Python<'_>) -> (f64, f64) {
    py.detach(|| fi_research::machine::factors(rayon::current_num_threads()))
}
