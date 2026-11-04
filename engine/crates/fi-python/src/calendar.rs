//! The economic clock: sorted, distinct times (`fi.Calendar`).

use super::*;

/// The economic clock: sorted, distinct times.
#[pyclass(name = "Calendar", module = "fi_engine._native", frozen)]
#[derive(Clone)]
pub struct PyCalendar {
    pub(crate) cal: Calendar,
}

#[pymethods]
impl PyCalendar {
    /// A calendar of the given times (nanoseconds, dates, datetimes or ISO dates).
    #[new]
    fn new(times: Vec<Bound<'_, PyAny>>) -> PyResult<Self> {
        Ok(Self { cal: Calendar::new(times.iter().map(convert::ts).collect::<PyResult<_>>()?) })
    }

    /// Every weekday from `start` to `end`, inclusive.
    #[staticmethod]
    fn weekdays(start: &Bound<'_, PyAny>, end: &Bound<'_, PyAny>) -> PyResult<Self> {
        Ok(Self { cal: Calendar::weekdays(convert::ts(start)?, convert::ts(end)?) })
    }

    fn __len__(&self) -> usize {
        self.cal.len()
    }

    /// The times, in nanoseconds.
    fn times(&self) -> Vec<i64> {
        self.cal.times().iter().map(|t| t.0).collect()
    }

    /// The times as dates.
    fn dates<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyList>> {
        PyList::new(py, self.cal.times().iter().map(|t| convert::date(py, *t)).collect::<PyResult<Vec<_>>>()?)
    }

    /// The last time of each month, in nanoseconds.
    fn month_ends(&self) -> Vec<i64> {
        self.cal.month_ends().iter().map(|t| t.0).collect()
    }

    /// Every `step`-th time from `offset`, in nanoseconds.
    #[pyo3(signature = (step, offset = 0))]
    fn every(&self, step: usize, offset: usize) -> Vec<i64> {
        self.cal.every(step, offset).iter().map(|t| t.0).collect()
    }

    /// The first `n` times.
    fn prefix(&self, n: usize) -> Self {
        Self { cal: self.cal.prefix(n) }
    }

    /// Times in either calendar.
    fn union(&self, other: &Self) -> Self {
        Self { cal: Calendar::union(&[&self.cal, &other.cal]) }
    }

    /// Times in both calendars.
    fn intersection(&self, other: &Self) -> Self {
        Self { cal: Calendar::intersection(&[&self.cal, &other.cal]) }
    }

    /// Content hash.
    #[getter]
    fn version(&self) -> u64 {
        self.cal.version()
    }

    fn __repr__(&self) -> String {
        match (self.cal.times().first(), self.cal.times().last()) {
            (Some(a), Some(b)) => format!("Calendar({} times, {a} to {b})", self.cal.len()),
            _ => "Calendar(empty)".into(),
        }
    }
}
