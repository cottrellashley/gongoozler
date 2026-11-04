//! Data across the boundary: Arrow PyCapsule in, Rust Polars inside, Python Polars out; and times.
//!
//! A frame arrives as anything with `__arrow_c_stream__` (Polars, pandas, pyarrow, DuckDB). Only
//! the declared columns are taken, and they reach Rust Polars without copying (the C Data
//! Interface: arrow-rs and polars-arrow define the same `#[repr(C)]` structs). Types Rust Polars is
//! not built with are cast first: dictionaries (categoricals) to their values, decimals to floats,
//! small integers to wider ones.

use std::sync::Arc;

use arrow_array::{ArrayRef, RecordBatch, make_array};
use arrow_schema::{DataType as A, Field as AField, Schema as ASchema, TimeUnit};
use polars::prelude::{Column, CompatLevel, DataFrame, Series};
use polars_arrow::{datatypes::Field as PlField, ffi as pffi};
use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3_arrow::PyTable;

use fi_kernel::{Ts, time::days_from_civil};

/// Any displayable error as a Python `ValueError`.
pub fn err<E: std::fmt::Display>(e: E) -> PyErr {
    PyValueError::new_err(e.to_string())
}

/// The type a column crosses as.
fn plain(t: &A) -> A {
    match t {
        A::Dictionary(_, v) => plain(v),
        A::Decimal32(..) | A::Decimal64(..) | A::Decimal128(..) | A::Decimal256(..) | A::Float16 => A::Float64,
        A::Int8 | A::Int16 => A::Int32,
        A::UInt8 | A::UInt16 => A::UInt32,
        A::Date64 => A::Timestamp(TimeUnit::Millisecond, None),
        A::Duration(_) => A::Int64,
        // Times are UTC instants: the zone is dropped without moving the values.
        A::Timestamp(u, Some(_)) => A::Timestamp(*u, None),
        other => other.clone(),
    }
}

fn prepare(a: &ArrayRef, to: &A) -> PyResult<ArrayRef> {
    if a.data_type() == to {
        return Ok(a.clone());
    }
    if let (A::Timestamp(..), A::Timestamp(..)) = (a.data_type(), to) {
        let d = a.to_data().into_builder().data_type(to.clone()).build().map_err(err)?;
        return Ok(make_array(d));
    }
    arrow_cast::cast(a, to).map_err(err)
}

/// The frame's columns `columns`, in that order, as a Rust Polars frame.
///
/// # Errors
/// A column the frame does not have, or a type that cannot cross.
pub fn frame(table: PyTable, columns: &[&str]) -> PyResult<DataFrame> {
    let (batches, schema) = table.into_inner();
    let have = || schema.fields().iter().map(|f| f.name().clone()).collect::<Vec<_>>();
    let idx: Vec<usize> = columns
        .iter()
        .map(|c| schema.index_of(c).map_err(|_| crate::UnknownError::new_err(format!("the frame has no column {c} (has {:?})", have()))))
        .collect::<PyResult<_>>()?;
    let fields: Vec<AField> = idx.iter().map(|&i| AField::new(schema.field(i).name(), plain(schema.field(i).data_type()), true)).collect();
    let out = Arc::new(ASchema::new(fields));
    let mut kept = Vec::with_capacity(batches.len().max(1));
    for b in &batches {
        let cols = idx.iter().zip(out.fields()).map(|(&i, f)| prepare(b.column(i), f.data_type())).collect::<PyResult<Vec<_>>>()?;
        kept.push(RecordBatch::try_new(out.clone(), cols).map_err(err)?);
    }
    if kept.is_empty() {
        kept.push(RecordBatch::new_empty(out.clone()));
    }
    let mut cols = Vec::with_capacity(out.fields().len());
    for (i, f) in out.fields().iter().enumerate() {
        let pf = field_to_polars(f)?;
        let chunks = kept.iter().map(|b| array_to_polars(b.column(i), &pf)).collect::<PyResult<Vec<_>>>()?;
        cols.push(Column::from(Series::try_from((&pf, chunks)).map_err(err)?));
    }
    DataFrame::new_infer_height(cols).map_err(err)
}

fn field_to_polars(f: &AField) -> PyResult<PlField> {
    let s = arrow_schema::ffi::FFI_ArrowSchema::try_from(f).map_err(err)?;
    // SAFETY: both crates define the C Data Interface's ArrowSchema as the same #[repr(C)] struct.
    let s: pffi::ArrowSchema = unsafe { std::mem::transmute(s) };
    // SAFETY: `s` is a valid, exported schema.
    unsafe { pffi::import_field_from_c(&s) }.map_err(err)
}

fn array_to_polars(a: &ArrayRef, f: &PlField) -> PyResult<Box<dyn polars_arrow::array::Array>> {
    let c = arrow_data::ffi::FFI_ArrowArray::new(&a.to_data());
    // SAFETY: as above, for ArrowArray; the import takes ownership of the exported array.
    let c: pffi::ArrowArray = unsafe { std::mem::transmute(c) };
    // SAFETY: `c` was exported with the type `f` describes.
    unsafe { pffi::import_array_from_c(c, f.dtype().clone()) }.map_err(err)
}

fn polars_to_arrow(f: &PlField, a: Box<dyn polars_arrow::array::Array>) -> PyResult<(AField, ArrayRef)> {
    let s = pffi::export_field_to_c(f);
    let c = pffi::export_array_to_c(a);
    // SAFETY: the same #[repr(C)] structs, the other way.
    let s: arrow_schema::ffi::FFI_ArrowSchema = unsafe { std::mem::transmute(s) };
    // SAFETY: as above.
    let c: arrow_data::ffi::FFI_ArrowArray = unsafe { std::mem::transmute(c) };
    // SAFETY: `c` and `s` were exported together.
    let data = unsafe { arrow_array::ffi::from_ffi(c, &s) }.map_err(err)?;
    Ok((AField::try_from(&s).map_err(err)?, make_array(data)))
}

/// A Rust Polars frame as a Python Polars frame, without copying the buffers.
///
/// # Errors
/// From the export, or if Python Polars is not installed.
pub fn to_python(py: Python<'_>, mut df: DataFrame) -> PyResult<Bound<'_, PyAny>> {
    if df.should_rechunk() {
        df.align_chunks();
    }
    let mut schema: Option<Arc<ASchema>> = None;
    let mut batches = Vec::new();
    for rb in df.iter_chunks(CompatLevel::newest(), false) {
        let pfields: Vec<PlField> = rb.schema().iter_values().cloned().collect();
        let mut fields = Vec::with_capacity(pfields.len());
        let mut arrays = Vec::with_capacity(pfields.len());
        for (pf, a) in pfields.iter().zip(rb.into_arrays()) {
            let (f, a) = polars_to_arrow(pf, a)?;
            fields.push(f);
            arrays.push(a);
        }
        let s = schema.get_or_insert_with(|| Arc::new(ASchema::new(fields))).clone();
        batches.push(RecordBatch::try_new(s, arrays).map_err(err)?);
    }
    let schema = schema.unwrap_or_else(|| Arc::new(ASchema::empty()));
    let table = Bound::new(py, PyTable::try_new(batches, schema)?)?;
    py.import("polars")?.getattr("DataFrame")?.call1((table,))
}

/// A time from Python: nanoseconds since the epoch (int), `datetime.date`, `datetime.datetime`
/// (naive is UTC), a pandas `Timestamp`, a NumPy `datetime64`, or an ISO date string.
///
/// # Errors
/// Anything else.
pub fn ts(obj: &Bound<'_, PyAny>) -> PyResult<Ts> {
    if let Ok(n) = obj.extract::<i64>() {
        return Ok(Ts(n));
    }
    if let Ok(s) = obj.extract::<String>() {
        return date_str(&s).ok_or_else(|| PyValueError::new_err(format!("not a date: {s:?} (expected YYYY-MM-DD)")));
    }
    let type_name = obj.get_type().name()?.to_string();
    if type_name == "datetime64" {
        return Ok(Ts(obj.call_method1("astype", ("datetime64[ns]",))?.call_method1("astype", ("int64",))?.extract()?));
    }
    if obj.hasattr("nanosecond")? && obj.hasattr("value")? {
        return Ok(Ts(obj.getattr("value")?.extract()?)); // pandas Timestamp: UTC nanoseconds
    }
    if obj.hasattr("year")? {
        let day = days_from_civil(obj.getattr("year")?.extract()?, obj.getattr("month")?.extract()?, obj.getattr("day")?.extract()?);
        let mut ns = day * Ts::DAY;
        if obj.hasattr("hour")? {
            let part = |n: &str| -> PyResult<i64> { obj.getattr(n)?.extract() };
            ns += ((part("hour")? * 60 + part("minute")?) * 60 + part("second")?) * 1_000_000_000 + part("microsecond")? * 1_000;
            let off = obj.call_method0("utcoffset")?;
            if !off.is_none() {
                let secs: f64 = off.call_method0("total_seconds")?.extract()?;
                ns -= (secs * 1e9).round() as i64;
            }
        }
        return Ok(Ts(ns));
    }
    Err(PyTypeError::new_err(format!("not a time: {type_name} (use nanoseconds, a date, a datetime or YYYY-MM-DD)")))
}

fn date_str(s: &str) -> Option<Ts> {
    let mut parts = s.trim().splitn(3, '-');
    let y: i32 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let d: u32 = parts.next()?.get(..2)?.parse().ok()?;
    ((1..=12).contains(&m) && (1..=31).contains(&d)).then(|| Ts::from_days(days_from_civil(y, m, d)))
}

/// A `datetime.date` for `t`.
///
/// # Errors
/// From Python.
pub fn date(py: Python<'_>, t: Ts) -> PyResult<Bound<'_, PyAny>> {
    let (y, m, d) = t.ymd();
    py.import("datetime")?.getattr("date")?.call1((y, m, d))
}
