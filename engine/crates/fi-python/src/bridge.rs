//! Python models and decisions: Python callables behind the kernel's traits.
//!
//! The kernel runs with the GIL released; a Python callable takes it for its own call only.
//! Python models evaluate on one thread ([`ModelSpec::serial`]), so their calls arrive in time
//! order. A Python exception stops the evaluation or the run, and the original exception is raised
//! again to the caller ([`Errors`]).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use numpy::{PyArray1, PyReadonlyArray1};
use pyo3::exceptions::{PyKeyError, PyRuntimeError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyTuple};

use fi_kernel::{CrossSectionModel, DecisionCtx, DecisionModel, EntityId, ModelSpec, Targets, Ts, UnitRecords, View, Workspace};

use crate::convert;

/// The first Python exception a bridge saw, kept so the caller can raise it unchanged.
#[derive(Default)]
pub struct Errors(Mutex<Option<PyErr>>);

impl Errors {
    /// Keeps `e` (if it is the first) and returns the kernel error that stops the work.
    pub fn fail(&self, e: PyErr, what: &str) -> fi_kernel::Error {
        let msg = format!("{what} raised {e}");
        let mut slot = self.0.lock().expect("not poisoned");
        if slot.is_none() {
            *slot = Some(e);
        }
        fi_kernel::Error::Invalid(msg)
    }

    /// The exception, if one was raised.
    pub fn take(&self) -> Option<PyErr> {
        self.0.lock().expect("not poisoned").take()
    }
}

static NEXT: AtomicU64 = AtomicU64::new(1);

/// A parameter hash no other Python model has: each Python model object is its own cache entry,
/// since the engine cannot see what a Python function depends on.
pub fn fresh() -> u64 {
    NEXT.fetch_add(1, Ordering::Relaxed).rotate_left(32) ^ 0x9e37_79b9_7f4a_7c15
}

/// Contract names by row, and rows by name.
pub struct Names {
    /// Names, in the records' row order.
    pub names: Vec<String>,
    /// Entities, in the same order.
    pub ids: Vec<EntityId>,
    by_name: HashMap<String, Option<EntityId>>,
}

impl Names {
    /// The names of `ids` in `ws`.
    pub fn new(ws: &Workspace, ids: &[EntityId]) -> Self {
        let names: Vec<String> = ids.iter().map(|e| ws.entities.name(*e).to_owned()).collect();
        let mut by_name: HashMap<String, Option<EntityId>> = HashMap::with_capacity(ids.len());
        for (n, e) in names.iter().zip(ids) {
            by_name.entry(n.clone()).and_modify(|v| *v = None).or_insert(Some(*e));
        }
        Self { names, ids: ids.to_vec(), by_name }
    }

    /// The entity named `name`.
    ///
    /// # Errors
    /// No such name, or two entities (of different kinds) share it.
    pub fn id(&self, name: &str) -> PyResult<EntityId> {
        match self.by_name.get(name) {
            Some(Some(e)) => Ok(*e),
            Some(None) => Err(PyValueError::new_err(format!("{name} names more than one contract (of different kinds)"))),
            None => Err(PyKeyError::new_err(format!("{name} is not a contract"))),
        }
    }
}

/// A one-dimensional float array from a NumPy array, a pandas or Polars series, or a list.
pub fn floats(obj: &Bound<'_, PyAny>) -> PyResult<Vec<f64>> {
    if let Ok(a) = obj.extract::<PyReadonlyArray1<'_, f64>>() {
        return Ok(a.as_slice().map_or_else(|_| a.as_array().to_vec(), <[f64]>::to_vec));
    }
    let a = obj.py().import("numpy")?.call_method1("asarray", (obj, "float64"))?;
    let a: PyReadonlyArray1<'_, f64> = a.extract().map_err(|_| PyTypeError::new_err("expected one-dimensional numbers"))?;
    Ok(a.as_array().to_vec())
}

/// A Python cross-sectional model: `compute(t, entities, x)` once per time.
///
/// `t` is the time in nanoseconds, `entities` the universe's names (the same list object every
/// call), `x` one NumPy array per input. It returns one array per output (a single array when
/// there is one output), or a dict keyed by output name.
pub struct PyCrossSection {
    /// The declaration.
    pub spec: ModelSpec,
    /// The callable.
    pub compute: Py<PyAny>,
    /// Entity names by id, set before each evaluation.
    pub names: RwLock<Arc<Vec<String>>>,
    universe: Mutex<Option<(Vec<EntityId>, Py<PyList>)>>,
    /// The first exception.
    pub errors: Errors,
}

impl PyCrossSection {
    /// A model around `compute`.
    pub fn new(spec: ModelSpec, compute: Py<PyAny>) -> Self {
        Self { spec, compute, names: RwLock::new(Arc::new(Vec::new())), universe: Mutex::new(None), errors: Errors::default() }
    }

    fn call(&self, py: Python<'_>, t: Ts, universe: &[EntityId], x: &[Vec<f64>]) -> PyResult<Vec<Vec<f64>>> {
        let names = {
            let mut cache = self.universe.lock().expect("not poisoned");
            match &*cache {
                Some((ids, list)) if ids.as_slice() == universe => list.clone_ref(py),
                _ => {
                    let all = self.names.read().expect("not poisoned").clone();
                    let list = PyList::new(py, universe.iter().map(|e| all.get(e.0 as usize).map_or("", String::as_str)))?.unbind();
                    *cache = Some((universe.to_vec(), list.clone_ref(py)));
                    list
                }
            }
        };
        let arrays = PyList::new(py, x.iter().map(|v| PyArray1::from_slice(py, v)))?;
        let out = self.compute.bind(py).call1((t.0, names, arrays))?;
        let outputs = &self.spec.outputs;
        if let Ok(d) = out.cast::<PyDict>() {
            return outputs
                .iter()
                .map(|o| d.get_item(o)?.ok_or_else(|| PyKeyError::new_err(format!("the result has no output {o}"))).and_then(|v| floats(&v)))
                .collect();
        }
        if outputs.len() == 1
            && let Ok(v) = floats(&out)
        {
            return Ok(vec![v]);
        }
        out.try_iter()?.map(|o| floats(&o?)).collect()
    }
}

impl CrossSectionModel for PyCrossSection {
    fn spec(&self) -> ModelSpec {
        self.spec.clone()
    }

    fn compute(&self, t: Ts, universe: &[EntityId], x: &[Vec<f64>]) -> fi_kernel::Result<Vec<Vec<f64>>> {
        Python::attach(|py| self.call(py, t, universe, x).map_err(|e| self.errors.fail(e, &format!("python model {}", self.spec.name))))
    }
}

/// A Python decision: `decide(ctx)` once per scheduled time, returning `None` (keep the book), a
/// mapping of contract name to weight, or `("weights" | "quantities", mapping)`.
pub struct PyDecision {
    /// Name.
    pub name: String,
    /// Parameter hash.
    pub params: u64,
    /// The callable.
    pub decide: Py<PyAny>,
    /// The contracts.
    pub contracts: Arc<Names>,
    /// The first exception.
    pub errors: Arc<Errors>,
}

impl PyDecision {
    fn call(&self, py: Python<'_>, ctx: &DecisionCtx<'_>) -> PyResult<Option<Targets>> {
        let alive = Arc::new(AtomicBool::new(true));
        let base = |e: EntityId| if e == EntityId::NONE { "base".to_owned() } else { ctx.view.ws.entities.name(e).to_owned() };
        let c = Context {
            ws: std::ptr::from_ref(ctx.view.ws),
            records: std::ptr::from_ref(ctx.records),
            t: ctx.t,
            known_by: ctx.view.known_by,
            k: ctx.k,
            nav: ctx.nav,
            holdings: ctx.holdings.to_vec(),
            orders: ctx.orders.iter().map(|o| (o.row, o.remaining, o.requested, o.policy, o.submitted.0)).collect(),
            cash: ctx.cash.iter().map(|(e, v)| (base(*e), *v)).collect(),
            policy: ctx.policy,
            notes: Mutex::new(Vec::new()),
            state: ctx.state.to_vec(),
            contracts: self.contracts.clone(),
            alive: alive.clone(),
        };
        let c = Bound::new(py, c)?;
        let r = self.decide.bind(py).call1((c.clone(),));
        alive.store(false, Ordering::Release);
        for n in c.get().notes.lock().map(|mut n| std::mem::take(&mut *n)).unwrap_or_default() {
            ctx.note(n);
        }
        targets(&r?, &self.contracts)
    }
}

impl DecisionModel for PyDecision {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn params(&self) -> u64 {
        self.params
    }

    fn decide(&self, ctx: &DecisionCtx<'_>) -> fi_kernel::Result<Option<Targets>> {
        Python::attach(|py| self.call(py, ctx).map_err(|e| self.errors.fail(e, &format!("python decision {}", self.name))))
    }
}

fn targets(r: &Bound<'_, PyAny>, c: &Names) -> PyResult<Option<Targets>> {
    if r.is_none() {
        return Ok(None);
    }
    let (kind, map) = match r.cast::<PyTuple>() {
        Ok(t) if t.len() == 2 => (t.get_item(0)?.extract::<String>()?, t.get_item(1)?),
        _ => ("weights".to_owned(), r.clone()),
    };
    let items = map.call_method0("items").map_err(|_| PyTypeError::new_err("a decision returns None, a mapping of contract to target, or (kind, mapping)"))?;
    let mut pairs = Vec::new();
    for item in items.try_iter()? {
        let (name, v): (String, f64) = item?.extract()?;
        pairs.push((c.id(&name)?, v));
    }
    match kind.as_str() {
        "weights" => Ok(Some(Targets::Weights(pairs))),
        "quantities" => Ok(Some(Targets::Quantities(pairs))),
        "orders" => Ok(Some(Targets::Orders(pairs))),
        other => Err(PyValueError::new_err(format!("targets are weights, quantities or orders, not {other}"))),
    }
}

/// What a Python decision sees: the time, the book, and point-in-time reads of any series. Valid
/// only during the call it was made for.
#[pyclass(name = "DecisionContext", module = "fi_engine._native", frozen)]
pub struct Context {
    ws: *const Workspace,
    records: *const UnitRecords,
    t: Ts,
    known_by: Ts,
    k: usize,
    nav: f64,
    holdings: Vec<(usize, f64)>,
    orders: Vec<(usize, f64, f64, usize, i64)>,
    cash: Vec<(String, f64)>,
    policy: usize,
    notes: Mutex<Vec<String>>,
    state: Vec<(String, f64)>,
    contracts: Arc<Names>,
    alive: Arc<AtomicBool>,
}

// SAFETY: the pointers are read only while `alive`, which is true only during the decision call.
// The ledger holds both borrows across that call, and every method holds the GIL (it never
// detaches), so the call cannot return, and the borrows cannot end, while a method reads them.
unsafe impl Send for Context {}
// SAFETY: as above; the referents are immutable and `Sync`.
unsafe impl Sync for Context {}

impl Context {
    fn parts(&self) -> PyResult<(&Workspace, &UnitRecords)> {
        if !self.alive.load(Ordering::Acquire) {
            return Err(PyRuntimeError::new_err("a decision context is valid only during its decision"));
        }
        // SAFETY: see the `Send` impl.
        Ok(unsafe { (&*self.ws, &*self.records) })
    }

    fn view(&self) -> PyResult<(View<'_>, &UnitRecords)> {
        let (ws, rec) = self.parts()?;
        Ok((View::new(ws, self.t, self.known_by), rec))
    }

    /// Entities of `series` by name (when given) or the contracts.
    fn entities(&self, ws: &Workspace, series: &fi_kernel::Series, names: Option<Vec<String>>) -> PyResult<Vec<EntityId>> {
        let Some(names) = names else { return Ok(self.contracts.ids.clone()) };
        let own = Names::new(ws, &series.entities());
        names.iter().map(|n| own.id(n).or_else(|_| self.contracts.id(n))).collect()
    }
}

#[pymethods]
impl Context {
    /// The decision time, in nanoseconds.
    #[getter]
    fn t(&self) -> i64 {
        self.t.0
    }

    /// The decision date.
    #[getter]
    fn date<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        convert::date(py, self.t)
    }

    /// The calendar index of the decision.
    #[getter]
    fn k(&self) -> usize {
        self.k
    }

    /// Net asset value before trading.
    #[getter]
    fn nav(&self) -> f64 {
        self.nav
    }

    /// Every contract's name, in the order `cross_section` and `values` return them.
    #[getter]
    fn contracts<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyList>> {
        PyList::new(py, &self.contracts.names)
    }

    /// The book: contract name to quantity.
    #[getter]
    fn holdings<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        for (i, q) in &self.holdings {
            d.set_item(&self.contracts.names[*i], q)?;
        }
        Ok(d)
    }

    /// Orders still working, from every policy: contract, remaining and requested units (signed),
    /// the policy that asked, and when.
    #[getter]
    fn orders<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyList>> {
        let out = PyList::empty(py);
        for (row, remaining, requested, policy, submitted) in &self.orders {
            let d = PyDict::new(py);
            d.set_item("contract", &self.contracts.names[*row])?;
            d.set_item("remaining", remaining)?;
            d.set_item("requested", requested)?;
            d.set_item("policy", (*policy != usize::MAX).then_some(*policy))?;
            d.set_item("submitted", submitted)?;
            out.append(d)?;
        }
        Ok(out)
    }

    /// Cash that can be spent now, by currency (settled cash less committed payables).
    #[getter]
    fn cash<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        for (c, v) in &self.cash {
            d.set_item(c, v)?;
        }
        Ok(d)
    }

    /// The deciding policy's position in its book.
    #[getter]
    fn policy(&self) -> usize {
        self.policy
    }

    /// A value of the book's own state, as its distributor last published it (a deal's
    /// `"reinvesting"` or `"event_of_default"`), or `None`.
    fn state(&self, key: &str) -> Option<f64> {
        self.state.iter().find(|(k, _)| k == key).map(|(_, v)| *v)
    }

    /// Records a note in the run's journal: an optimiser's status, the constraint that binds.
    fn note(&self, text: String) {
        if let Ok(mut n) = self.notes.lock() {
            n.push(text);
        }
    }

    /// The latest value of `series.column` for `entity` known now, or `None`.
    fn value(&self, series: &str, column: &str, entity: &str) -> PyResult<Option<f64>> {
        let (view, _) = self.view()?;
        let s = view.series(series).map_err(crate::to_py)?;
        let c = s.column(column).map_err(crate::to_py)?;
        let e = self.entities(view.ws, s, Some(vec![entity.to_owned()]))?[0];
        Ok(view.asof(s, c, e).map(|h| h.value))
    }

    /// `series.column` for many entities as known now: the contracts (default) or the named
    /// entities of the series. NaN where there is no value.
    #[pyo3(signature = (series, column, entities=None))]
    fn cross_section<'py>(&self, py: Python<'py>, series: &str, column: &str, entities: Option<Vec<String>>) -> PyResult<Bound<'py, PyArray1<f64>>> {
        let (view, _) = self.view()?;
        let s = view.series(series).map_err(crate::to_py)?;
        let c = s.column(column).map_err(crate::to_py)?;
        let ids = self.entities(view.ws, s, entities)?;
        Ok(PyArray1::from_vec(py, view.cross_section(s, c, &ids)))
    }

    /// Every contract's unit value now.
    fn values<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyArray1<f64>>> {
        let (_, rec) = self.view()?;
        Ok(PyArray1::from_vec(py, (0..rec.entities().len()).map(|i| rec.value[rec.at(i, self.k)]).collect()))
    }

    /// A contract's last `n` unit returns known now (oldest first).
    fn returns<'py>(&self, py: Python<'py>, contract: &str, n: usize) -> PyResult<Bound<'py, PyArray1<f64>>> {
        let (_, rec) = self.view()?;
        let i = rec.idx(self.contracts.id(contract)?).ok_or_else(|| PyKeyError::new_err(format!("{contract} has no records")))?;
        Ok(PyArray1::from_vec(py, rec.returns_known(i, self.k, n, self.t)))
    }

    fn __repr__(&self) -> String {
        format!("DecisionContext(t={}, k={}, nav={:.2}, holdings={})", self.t, self.k, self.nav, self.holdings.len())
    }
}
