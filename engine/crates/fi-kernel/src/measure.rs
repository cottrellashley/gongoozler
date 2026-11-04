//! Measures: linear quantities per unit (exposure units).
//!
//! Exposure is Σ quantity × measure, sizing is target ÷ measure, and turnover can be counted in
//! any measure: market value, face, DV01, DTS, key rates, or anything a series supplies.

use crate::{records::UnitRecords, view::View};

/// A quantity per unit of a contract, linear in holdings.
pub trait Measure: Send + Sync {
    /// Name.
    fn name(&self) -> String;
    /// The measure for one unit of row `i` at calendar index `k`; `None` when unknown.
    fn per_unit(&self, records: &UnitRecords, i: usize, k: usize, view: &View<'_>) -> Option<f64>;
}

/// The unit's value: market-value exposure.
#[derive(Copy, Clone, Debug, Default)]
pub struct Value;

impl Measure for Value {
    fn name(&self) -> String {
        "value".into()
    }

    fn per_unit(&self, records: &UnitRecords, i: usize, k: usize, _view: &View<'_>) -> Option<f64> {
        let v = records.value[records.at(i, k)];
        v.is_finite().then_some(v)
    }
}

/// Principal outstanding per unit (current face per unit of original face): par exposure, as a
/// coverage test or a par constraint counts it.
#[derive(Clone, Copy, Debug, Default)]
pub struct Face;

impl Measure for Face {
    fn name(&self) -> String {
        "face".into()
    }

    fn per_unit(&self, records: &UnitRecords, i: usize, k: usize, _view: &View<'_>) -> Option<f64> {
        let f = records.face[records.at(i, k)];
        f.is_finite().then_some(f)
    }
}

/// A per-unit quantity read from a series: a vendor's DV01 or DTS per unit, or a model's output.
#[derive(Clone, Debug)]
pub struct FromSeries {
    /// Series name.
    pub series: String,
    /// Column name.
    pub column: String,
}

impl Measure for FromSeries {
    fn name(&self) -> String {
        format!("{}.{}", self.series, self.column)
    }

    fn per_unit(&self, records: &UnitRecords, i: usize, _k: usize, view: &View<'_>) -> Option<f64> {
        view.value(&self.series, &self.column, records.entities()[i])
    }
}

/// Exposure of `holdings` (row, quantity) in measure `m` at calendar index `k`.
#[must_use]
pub fn exposure(holdings: &[(usize, f64)], m: &dyn Measure, records: &UnitRecords, k: usize, view: &View<'_>) -> f64 {
    holdings.iter().filter_map(|(i, q)| m.per_unit(records, *i, k, view).map(|x| q * x)).sum()
}
