//! Python bindings: the `fi_engine._native` module, a module per Python object.
//!
//! * [`workspace`]: the `Workspace` of series (any Arrow frame: Polars, pandas, pyarrow, DuckDB,
//!   checked against its declaration; the engine never reads data itself), entities, models,
//!   contracts, records, runs and sweeps, deals and worlds;
//! * [`calendar`], [`model`] (inputs and models, built in or Python callables, [`bridge`]),
//!   [`records`] (contracts, unit records and manifests), [`decision`], [`fill`] and [`run`];
//! * [`terms`]: debt from its terms: business calendars, coupons, bonds, curves and analytics;
//! * [`book`]: books, policies, execution processes, constraints and agreements; [`deal`]: deals;
//! * [`convert`]: data across the boundary; [`progress`]: the watcher's ring.
//!
//! Every long computation runs with the GIL released, and under `gongoozler run` reports its
//! progress into the watcher's ring. The `fi_engine` Python package adds a few helpers on top
//! (`python/fi_engine`).

mod book;
mod bridge;
mod calendar;
mod convert;
mod deal;
mod decision;
mod fill;
mod model;
mod progress;
mod records;
mod run;
mod terms;
mod workspace;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use pyo3::create_exception;
use pyo3::exceptions::{PyException, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use pyo3_arrow::PyTable;

use fi_exec::{BookFill, Level, RfqFill};
use fi_io::{Known, SeriesDecl, ValueSpec};
use fi_kernel::{
    Calendar, Composite, Contract, DecisionModel, Entities, EntityId, Error, FillModel, Graph, InCurrency, Information, Input, KnowledgeBasis, KnowledgePolicy,
    Layout, LedgerOutput, MissingPolicy, ModelRole, ModelSpec, Outcomes, Producer, ResearchFill, Role, Shape, Transition, Ts, Unit, UnitRecords, Vintaged,
    Workspace, hash::Hash64,
};
use fi_research::Book;
use fi_std::{
    Branch, CashAccount, DailyFuture, Defaults, EwmaVol, GaussianPaths, LoanTerms, Merton, NelsonSiegel, Reinvest, Replay, RiskParity, RiskParityMethod,
    SuppliedBond, TopN, TotalReturn, Trades, ZScore, synth,
};

use book::{PyAgreement, PyBook, PyConstraint, PyExecution, PyPolicy};
use bridge::{Errors, Names, PyCrossSection, PyDecision};
use calendar::*;
use decision::*;
use fill::*;
use model::*;
use records::*;
use run::*;
use workspace::*;

create_exception!(fi_engine, EngineError, PyException, "An error from the engine.");
create_exception!(fi_engine, InvalidError, EngineError, "Input that breaks its contract: types, duplicates, NaN, a broken identity.");
create_exception!(fi_engine, UnknownError, EngineError, "A named series, column, entity or contract that does not exist.");
create_exception!(fi_engine, MissingError, EngineError, "A value the run needs is missing, and the policy says fail.");

/// A kernel error as a Python exception.
pub(crate) fn to_py(e: Error) -> PyErr {
    match e {
        Error::Invalid(m) => InvalidError::new_err(m),
        Error::Unknown(m) => UnknownError::new_err(m),
        Error::Missing(m) => MissingError::new_err(m),
    }
}

/// A contract in a currency, if one is named.
fn in_currency(c: Arc<dyn Contract>, ccy: Option<EntityId>) -> Arc<dyn Contract> {
    match ccy {
        Some(currency) => Arc::new(InCurrency { contract: c, currency }),
        None => c,
    }
}

/// The first Python exception any bridge kept, else the kernel's error.
fn raised(e: Error, errors: &[&Errors]) -> PyErr {
    errors.iter().find_map(|x| x.take()).unwrap_or_else(|| to_py(e))
}

fn days(d: f64) -> i64 {
    (d * Ts::DAY as f64).round() as i64
}

/// A transition from Python: `(at, from, {into: units}, cash, role, note)` (`fi.transition`).
fn transition(obj: &Bound<'_, PyAny>, names: &Names) -> PyResult<Transition> {
    let (at, from, into, cash, role, note): (Bound<'_, PyAny>, String, HashMap<String, f64>, f64, String, String) =
        obj.extract().map_err(|_| PyTypeError::new_err("a transition is fi.transition(at, from_, into, cash=..., role=..., note=...)"))?;
    let role = match role.as_str() {
        "principal" => Role::Principal,
        "recovery" => Role::Recovery,
        "exchange" => Role::Exchange,
        "income" => Role::Income,
        "price" => Role::Performance,
        "fee" => Role::Fee,
        r => return Err(PyValueError::new_err(format!("a transition books to principal, recovery, exchange, income, price or fee, not {r}"))),
    };
    let mut into: Vec<(EntityId, f64)> = into.iter().map(|(n, r)| Ok((names.id(n)?, *r))).collect::<PyResult<_>>()?;
    into.sort_by_key(|(e, _)| e.0);
    Ok(Transition { at: convert::ts(&at)?, from: names.id(&from)?, into, cash, role, note })
}

fn split(spec: &str) -> PyResult<(&str, &str)> {
    spec.split_once('.').ok_or_else(|| PyValueError::new_err(format!("expected series.column, not {spec:?}")))
}

#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyWorkspace>()?;
    m.add_class::<PyCalendar>()?;
    m.add_class::<PyInput>()?;
    m.add_class::<PyModel>()?;
    m.add_class::<PyContracts>()?;
    m.add_class::<PyRecords>()?;
    m.add_class::<PyDecisionSpec>()?;
    m.add_class::<PyFill>()?;
    m.add_class::<PyPolicy>()?;
    m.add_class::<PyBook>()?;
    m.add_class::<PyExecution>()?;
    m.add_class::<PyConstraint>()?;
    m.add_class::<PyAgreement>()?;
    m.add_class::<PyManifest>()?;
    m.add_class::<PyRun>()?;
    m.add_class::<bridge::Context>()?;
    m.add_class::<terms::PyBond>()?;
    m.add_class::<terms::PyCoupon>()?;
    m.add_class::<terms::PyBusinessCalendar>()?;
    m.add_class::<terms::PyCurve>()?;
    let py = m.py();
    m.add("EngineError", py.get_type::<EngineError>())?;
    m.add("InvalidError", py.get_type::<InvalidError>())?;
    m.add("UnknownError", py.get_type::<UnknownError>())?;
    m.add("MissingError", py.get_type::<MissingError>())?;
    m.add("DAY", Ts::DAY)?;
    // Under a supervisor, the engine reports its progress into a ring of its own, closed at exit.
    progress::install();
    m.add_function(wrap_pyfunction!(progress::_close_progress, m)?)?;
    m.add_function(wrap_pyfunction!(machine_factors, m)?)?;
    py.import("atexit")?.call_method1("register", (m.getattr("_close_progress")?,))?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
