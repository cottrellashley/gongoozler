//! Contracts, unit records and manifests (`fi.Contracts`, `fi.Records`, `fi.Manifest`).

use super::*;

/// A set of contracts (instruments), to step into unit records.
#[pyclass(name = "Contracts", module = "fi_engine._native", frozen)]
#[derive(Clone)]
pub struct PyContracts {
    pub(crate) inner: Vec<Arc<dyn Contract>>,
}

/// A run's manifest (spec, change 7): equal manifests give equal outputs, and `diff` attributes
/// any difference to the series, models and settings that changed.
#[pyclass(name = "Manifest", module = "fi_engine._native", frozen)]
pub struct PyManifest {
    pub(crate) m: fi_research::Manifest,
}

#[pymethods]
impl PyManifest {
    /// A hash of everything that determines the output.
    #[getter]
    fn fingerprint(&self) -> u64 {
        self.m.fingerprint()
    }

    /// The run's mode.
    #[getter]
    fn mode(&self) -> String {
        self.m.mode.clone()
    }

    /// What differs from `other`, attributed: one line per series, model, setting or output.
    fn diff(&self, other: &Self) -> Vec<String> {
        self.m.diff(&other.m)
    }

    /// The manifest as JSON.
    fn json(&self) -> String {
        self.m.to_json()
    }

    /// The manifest as a dict.
    fn as_dict<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        py.import("json")?.call_method1("loads", (self.m.to_json(),))
    }

    fn __eq__(&self, other: &Self) -> bool {
        self.m == other.m
    }

    fn __repr__(&self) -> String {
        format!(
            "Manifest({}, {} series, {} models, mode={}, fingerprint={:016x})",
            self.m.book.0,
            self.m.series.len(),
            self.m.models.len(),
            self.m.mode,
            self.m.fingerprint()
        )
    }
}

#[pymethods]
impl PyContracts {
    /// What these contracts' definitions model, generated from the definitions: a Polars frame of
    /// kind and feature.
    fn support_matrix<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let m = fi_research::support_matrix(&self.inner, &[]);
        let d = PyDict::new(py);
        d.set_item("kind", m.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>())?;
        d.set_item("feature", m.iter().map(|(_, f)| f.clone()).collect::<Vec<_>>())?;
        py.import("polars")?.call_method1("DataFrame", (d,))
    }

    fn __len__(&self) -> usize {
        self.inner.len()
    }

    fn __add__(&self, other: &Self) -> Self {
        Self { inner: self.inner.iter().chain(&other.inner).cloned().collect() }
    }

    fn __repr__(&self) -> String {
        let mut kinds: Vec<&str> = self.inner.iter().map(|c| c.kind()).collect();
        kinds.sort_unstable();
        kinds.dedup();
        format!("Contracts({}, kinds={kinds:?})", self.inner.len())
    }
}

/// Unit records: every contract's value, cash and P&L terms per unit at every calendar time.
#[pyclass(name = "Records", module = "fi_engine._native", frozen)]
pub struct PyRecords {
    pub(crate) rec: Arc<UnitRecords>,
    pub(crate) names: Arc<Names>,
    pub(crate) diagnostics: (u64, u64, u64, u64),
    /// The series the contracts read: the world a run on these records is in.
    pub(crate) reads: Arc<Vec<String>>,
    /// Approximations declared when they were prepared.
    pub(crate) approximations: Arc<Vec<String>>,
}

#[pymethods]
impl PyRecords {
    /// The approximations declared when they were prepared.
    #[getter]
    fn approximations(&self) -> Vec<String> {
        self.approximations.to_vec()
    }

    fn __len__(&self) -> usize {
        self.rec.entities().len()
    }

    /// Contract names, in row order.
    #[getter]
    fn contracts(&self) -> Vec<String> {
        self.names.names.clone()
    }

    /// The calendar.
    #[getter]
    fn calendar(&self) -> PyCalendar {
        PyCalendar { cal: self.rec.calendar().clone() }
    }

    /// What the contracts did not observe: values carried stale, defaulted, missing.
    #[getter]
    fn diagnostics<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        d.set_item("stale", self.diagnostics.0)?;
        d.set_item("defaults", self.diagnostics.1)?;
        d.set_item("missing", self.diagnostics.2)?;
        Ok(d)
    }

    /// Content hash.
    #[getter]
    fn version(&self) -> u64 {
        self.rec.version()
    }

    /// The records as a long Polars frame: contract, time, value, cash, terms, known, state.
    fn frame<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let names: Vec<&str> = self.names.names.iter().map(String::as_str).collect();
        convert::to_python(py, fi_io::records_frame_named(&self.rec, &names).map_err(convert::err)?)
    }

    fn __repr__(&self) -> String {
        format!("Records({} contracts × {} times)", self.rec.entities().len(), self.rec.calendar().len())
    }
}
