//! Polars at the edges.
//!
//! * [`register`]: a Polars frame plus a [`SeriesDecl`] becomes a kernel series. The declaration
//!   is the contract (`kb/native/usage.md`): which column names the entity, which the event
//!   time, how the knowledge time is found, and each value's unit. Everything is checked when the
//!   frame arrives (principle 7): types, nulls in keys, NaN and infinity, duplicates.
//! * [`ledger_frame`] and [`series_frame`]: results go back out as Polars frames.
//!
//! The engine never reads data itself: the frame came from wherever the user got it.

use std::collections::HashMap;
use std::sync::Arc;

use fi_kernel::{
    Cause, Column as KColumn, Entities, EntityId, Error, Intervention, KnowledgeBasis, LedgerOutput, Producer, Provenance, Result, Series as KSeries, Ts, Unit,
    UnitRecords, Workspace,
};
use polars::prelude::*;

/// How a series' knowledge times are found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Known {
    /// A column of knowledge times (a publication timestamp, a filing date).
    Column(String),
    /// The event time plus a fixed lag, in nanoseconds.
    Lag(i64),
    /// Known at the event time of the `n`-th later row of the same entity, as the legacy
    /// frameworks count lags in data rows. The last `n` rows are never known.
    Rows(usize),
    /// Known at the event time.
    Same,
}

/// One value column, its unit, and the factor that converts it (0.01 for a percent rate).
#[derive(Clone, Debug, PartialEq)]
pub struct ValueSpec {
    /// Column name in the frame (and in the series).
    pub column: String,
    /// Unit, after conversion.
    pub unit: Unit,
    /// Multiplier applied on arrival.
    pub scale: f64,
    /// For a text column of [`Unit::Ref`] or [`Unit::Category`]: the kind its values are interned
    /// as (the referenced entity's kind, or `"category"`). The series stores the entity ids.
    pub kind: Option<String>,
}

/// A series declaration: the contract a frame must meet.
#[derive(Clone, Debug)]
pub struct SeriesDecl {
    /// Series name.
    pub name: String,
    /// (column, entity kind). `None`: a market-wide series.
    pub entity: Option<(String, String)>,
    /// Event time column (Date or Datetime). `None`: static attributes, always true.
    pub time: Option<String>,
    /// Knowledge times.
    pub known: Known,
    /// Value columns.
    pub values: Vec<ValueSpec>,
    /// Store as a dense grid (every entity at every time) rather than rows.
    pub grid: bool,
    /// Treat NaN as missing instead of refusing it (pandas producers use NaN for missing).
    pub nan_is_missing: bool,
    /// The state the series describes (for example `"clo1.tranche_flows"`), if declared.
    pub describes: Option<String>,
    /// A scenario: values that did not happen (a stress, an alternative trade list), named. Every
    /// series derived from it inherits the label (spec, change 4).
    pub scenario: Option<String>,
}

impl SeriesDecl {
    /// A declaration with no columns yet, known at the event time.
    #[must_use]
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            entity: None,
            time: None,
            known: Known::Same,
            values: Vec::new(),
            grid: false,
            nan_is_missing: false,
            describes: None,
            scenario: None,
        }
    }

    /// The entity column and its kind.
    #[must_use]
    pub fn entity(mut self, column: &str, kind: &str) -> Self {
        self.entity = Some((column.to_owned(), kind.to_owned()));
        self
    }

    /// The event time column.
    #[must_use]
    pub fn time(mut self, column: &str) -> Self {
        self.time = Some(column.to_owned());
        self
    }

    /// How knowledge times are found.
    #[must_use]
    pub fn known(mut self, known: Known) -> Self {
        self.known = known;
        self
    }

    /// A value column.
    #[must_use]
    pub fn value(mut self, column: &str, unit: Unit) -> Self {
        self.values.push(ValueSpec { column: column.to_owned(), unit, scale: 1.0, kind: None });
        self
    }

    /// A rate in percent, converted to a fraction.
    #[must_use]
    pub fn percent(mut self, column: &str) -> Self {
        self.values.push(ValueSpec { column: column.to_owned(), unit: Unit::Rate, scale: 0.01, kind: None });
        self
    }

    /// A rate in basis points, converted to a fraction.
    #[must_use]
    pub fn bp(mut self, column: &str) -> Self {
        self.values.push(ValueSpec { column: column.to_owned(), unit: Unit::Rate, scale: 1e-4, kind: None });
        self
    }

    /// A relationship: each value names an entity of `kind` (a bond's issuer, an index member's
    /// index). Text is interned; numbers are taken as entity ids already.
    #[must_use]
    pub fn reference(mut self, column: &str, kind: &str) -> Self {
        self.values.push(ValueSpec { column: column.to_owned(), unit: Unit::Ref, scale: 1.0, kind: Some(kind.to_owned()) });
        self
    }

    /// A category (a sector, a rating, a currency). Text is interned as entities of kind
    /// `"category"`, so equal labels get equal codes across series; numbers are taken as codes.
    #[must_use]
    pub fn category(mut self, column: &str) -> Self {
        self.values.push(ValueSpec { column: column.to_owned(), unit: Unit::Category, scale: 1.0, kind: Some("category".to_owned()) });
        self
    }

    /// Store as a grid.
    #[must_use]
    pub fn grid(mut self) -> Self {
        self.grid = true;
        self
    }

    /// Read NaN as missing.
    #[must_use]
    pub fn nan_is_missing(mut self) -> Self {
        self.nan_is_missing = true;
        self
    }

    /// Declares the state the series describes.
    #[must_use]
    pub fn describes(mut self, state: &str) -> Self {
        self.describes = Some(state.to_owned());
        self
    }
}

fn polars_err(context: &str, e: &PolarsError) -> Error {
    Error::Invalid(format!("{context}: {e}"))
}

fn column<'a>(df: &'a DataFrame, name: &str, decl: &str) -> Result<&'a Series> {
    df.column(name).map(|c| c.as_materialized_series()).map_err(|_| {
        let have: Vec<String> = df.get_column_names().iter().map(|n| n.to_string()).collect();
        Error::Unknown(format!("series {decl}: the frame has no column {name} (has {have:?})"))
    })
}

/// Event or knowledge times, in nanoseconds, from a Date or Datetime column. Nulls are refused.
fn times(s: &Series, decl: &str) -> Result<Vec<Ts>> {
    let (factor, ints) = match s.dtype() {
        DataType::Date => (Ts::DAY, s.cast(&DataType::Int64)),
        DataType::Datetime(tu, _) => (
            match tu {
                TimeUnit::Nanoseconds => 1,
                TimeUnit::Microseconds => 1_000,
                TimeUnit::Milliseconds => 1_000_000,
            },
            s.cast(&DataType::Int64),
        ),
        other => return Err(Error::Invalid(format!("series {decl}: column {} must be a Date or Datetime, not {other}", s.name()))),
    };
    let ints = ints.map_err(|e| polars_err(decl, &e))?;
    let ca = ints.i64().map_err(|e| polars_err(decl, &e))?;
    ca.iter()
        .enumerate()
        .map(|(i, v)| v.map(|x| Ts(x * factor)).ok_or_else(|| Error::Invalid(format!("series {decl}: column {} is null at row {i}", s.name()))))
        .collect()
}

/// Registers `df` as a series, checked against `decl`.
///
/// # Errors
/// Missing columns, wrong types, nulls in keys or times, infinite values, NaN (unless declared
/// missing), duplicate keys, or a name already in use.
pub fn register(ws: &mut Workspace, df: &DataFrame, decl: &SeriesDecl) -> Result<Arc<KSeries>> {
    let name = decl.name.as_str();
    let n = df.height();
    let entity: Vec<EntityId> = match &decl.entity {
        Some((col, kind)) => {
            let s = column(df, col, name)?.cast(&DataType::String).map_err(|e| polars_err(name, &e))?;
            let ca = s.str().map_err(|e| polars_err(name, &e))?;
            ca.iter()
                .enumerate()
                .map(|(i, v)| v.map(|x| ws.entities.intern(kind, x)).ok_or_else(|| Error::Invalid(format!("series {name}: entity is null at row {i}"))))
                .collect::<Result<_>>()?
        }
        None => vec![EntityId::NONE; n],
    };
    let event: Vec<Ts> = match &decl.time {
        Some(col) => times(column(df, col, name)?, name)?,
        None => vec![Ts::MIN; n],
    };
    let known: Vec<Ts> = match &decl.known {
        Known::Column(col) => times(column(df, col, name)?, name)?,
        Known::Lag(nanos) => event.iter().map(|t| t.plus(*nanos)).collect(),
        Known::Same => event.clone(),
        Known::Rows(lag) => rows_lag(&entity, &event, *lag),
    };
    let mut columns = Vec::with_capacity(decl.values.len());
    for spec in &decl.values {
        let s = column(df, &spec.column, name)?;
        if let (Some(kind), DataType::String) = (&spec.kind, s.dtype()) {
            let ca = s.str().map_err(|e| polars_err(name, &e))?;
            let values = ca.iter().map(|v| v.map_or(f64::NAN, |x| ws.entities.intern(kind, x).as_value())).collect();
            columns.push(KColumn::new(spec.column.clone(), spec.unit, values));
            continue;
        }
        let f = s.cast(&DataType::Float64).map_err(|e| polars_err(name, &e))?;
        let ca = f.f64().map_err(|e| polars_err(name, &e))?;
        let mut values = Vec::with_capacity(n);
        for (i, v) in ca.iter().enumerate() {
            let x = match v {
                None => f64::NAN,
                Some(x) if x.is_nan() && !decl.nan_is_missing => {
                    return Err(Error::Invalid(format!(
                        "series {name}: column {} is NaN at row {i}; send null for missing, or declare nan_is_missing",
                        spec.column
                    )));
                }
                Some(x) if x.is_infinite() => return Err(Error::Invalid(format!("series {name}: column {} is infinite at row {i}", spec.column))),
                Some(x) => x * spec.scale,
            };
            values.push(x);
        }
        columns.push(KColumn::new(spec.column.clone(), spec.unit, values));
    }
    let series = if decl.grid { to_grid(name, &entity, &event, &known, columns)? } else { KSeries::rows(name, entity, event, known, columns)? };
    let knowledge = match &decl.known {
        Known::Column(_) => KnowledgeBasis::Observed,
        Known::Lag(n) => KnowledgeBasis::Assumed(format!("event time + {} h", *n as f64 / 3.6e12)),
        Known::Rows(n) => KnowledgeBasis::Assumed(format!("the event time of the {n}th later row")),
        Known::Same => KnowledgeBasis::Assumed("the event time".into()),
    };
    let intervention = decl.scenario.clone().map(Intervention::Scenario);
    ws.add_series(series.with_provenance(Provenance { knowledge, producer: Producer::Observed, describes: decl.describes.clone(), intervention }))
}

fn rows_lag(entity: &[EntityId], event: &[Ts], lag: usize) -> Vec<Ts> {
    let mut by: HashMap<EntityId, Vec<(Ts, usize)>> = HashMap::new();
    for (i, (e, t)) in entity.iter().zip(event).enumerate() {
        by.entry(*e).or_default().push((*t, i));
    }
    let mut known = vec![Ts::MAX; entity.len()];
    for rows in by.values_mut() {
        rows.sort_unstable();
        for (p, (_, i)) in rows.iter().enumerate() {
            if let Some((t, _)) = rows.get(p + lag) {
                known[*i] = *t;
            }
        }
    }
    known
}

fn to_grid(name: &str, entity: &[EntityId], event: &[Ts], known: &[Ts], columns: Vec<KColumn>) -> Result<KSeries> {
    let mut times: Vec<Ts> = event.to_vec();
    times.sort_unstable();
    times.dedup();
    let mut entities: Vec<EntityId> = entity.to_vec();
    entities.sort_unstable();
    entities.dedup();
    let tpos: HashMap<Ts, usize> = times.iter().enumerate().map(|(i, t)| (*t, i)).collect();
    let epos: HashMap<EntityId, usize> = entities.iter().enumerate().map(|(i, e)| (*e, i)).collect();
    let (tn, en) = (times.len(), entities.len());
    let mut kn = vec![Ts::MIN; tn];
    let mut seen = vec![false; tn * en];
    let mut cols: Vec<Vec<f64>> = vec![vec![f64::NAN; tn * en]; columns.len()];
    for i in 0..entity.len() {
        let (ti, ej) = (tpos[&event[i]], epos[&entity[i]]);
        let at = ti * en + ej;
        if seen[at] {
            return Err(Error::Invalid(format!("series {name}: two rows for one entity at {}; a grid holds one vintage", event[i])));
        }
        seen[at] = true;
        kn[ti] = kn[ti].max(known[i]);
        for (c, src) in cols.iter_mut().zip(&columns) {
            c[at] = src.values[i];
        }
    }
    let columns = columns.into_iter().zip(cols).map(|(c, v)| KColumn::new(c.name, c.unit, v)).collect();
    KSeries::grid(name, entities, times, kn, columns)
}

fn datetime(name: &str, ts: impl Iterator<Item = i64>) -> PolarsResult<Series> {
    Series::new(name.into(), ts.collect::<Vec<i64>>()).cast(&DataType::Datetime(TimeUnit::Nanoseconds, None))
}

/// A run's accounts as a frame: one row per calendar time.
///
/// # Errors
/// From Polars.
pub fn ledger_frame(out: &LedgerOutput) -> PolarsResult<DataFrame> {
    let f = |name: &str, v: &[f64]| Series::new(name.into(), v.to_vec()).into_column();
    DataFrame::new_infer_height(vec![
        datetime("time", out.times.iter().map(|t| t.0))?.into_column(),
        f("nav", &out.nav),
        f("cash", &out.cash),
        f("income", &out.income),
        f("price", &out.price),
        f("principal", &out.principal),
        f("other", &out.other),
        f("financing", &out.financing),
        f("costs", &out.costs),
        f("currency", &out.currency),
        f("capital", &out.capital),
        f("unsettled", &out.unsettled),
        f("overdue", &out.overdue),
        f("nav_per_unit", &out.nav_per_unit),
        f("turnover", &out.turnover),
        Series::new("holdings".into(), out.holdings.iter().map(|h| *h as u64).collect::<Vec<u64>>()).into_column(),
    ])
}

fn name_of(entities: &Entities, e: EntityId) -> Option<String> {
    (e != EntityId::NONE).then(|| entities.name(e).to_owned())
}

/// A run's journal as a frame: every transfer with its cause.
///
/// # Errors
/// From Polars.
pub fn journal_frame(out: &LedgerOutput, entities: &Entities) -> PolarsResult<DataFrame> {
    let j = &out.journal;
    let cause = |c: &Cause| match c {
        Cause::Policy(p) => format!("policy {p}"),
        Cause::Order(id) => format!("order {id}"),
        Cause::Contract(e) => format!("contract {}", entities.name(*e)),
        Cause::Due(id) => format!("due {id}"),
        Cause::Capital => "capital".into(),
        Cause::Interest => "interest".into(),
        Cause::Horizon => "horizon".into(),
        Cause::Agreement(a) => format!("agreement {a}"),
        Cause::Book => "book".into(),
        Cause::Transition(n) => format!("transition {n}"),
    };
    DataFrame::new_infer_height(vec![
        Series::new("id".into(), j.iter().map(|e| e.id).collect::<Vec<u64>>()).into_column(),
        datetime("time", j.iter().map(|e| e.t.0))?.into_column(),
        Series::new("kind".into(), j.iter().map(|e| format!("{:?}", e.kind).to_lowercase()).collect::<Vec<String>>()).into_column(),
        Series::new("entity".into(), j.iter().map(|e| name_of(entities, e.entity)).collect::<Vec<Option<String>>>()).into_column(),
        Series::new("currency".into(), j.iter().map(|e| name_of(entities, e.currency)).collect::<Vec<Option<String>>>()).into_column(),
        Series::new("qty".into(), j.iter().map(|e| e.qty).collect::<Vec<f64>>()).into_column(),
        Series::new("amount".into(), j.iter().map(|e| e.amount).collect::<Vec<f64>>()).into_column(),
        Series::new("cause".into(), j.iter().map(|e| cause(&e.cause)).collect::<Vec<String>>()).into_column(),
        Series::new("note".into(), j.iter().map(|e| e.note.clone()).collect::<Vec<Option<String>>>()).into_column(),
    ])
}

/// A run's financing agreements as a frame: one row per agreement and time, with the balance,
/// what the collateral supports (`capacity`; a CSA: the netting set's value), the margin the
/// lender `called` at that time, what the book `paid` against calls, the `call` still
/// outstanding after paying, and the collateral's `value` (a CSA: the collateral required).
///
/// # Errors
/// From Polars.
pub fn financing_frame(out: &LedgerOutput) -> PolarsResult<DataFrame> {
    let n = out.times.len();
    let mut name = Vec::new();
    let mut time = Vec::new();
    let (mut balance, mut capacity, mut call, mut value) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let (mut called, mut paid) = (Vec::new(), Vec::new());
    for a in &out.agreements {
        for k in 0..n.min(a.balance.len()) {
            name.push(a.name.clone());
            time.push(out.times[k].0);
            balance.push(a.balance[k]);
            capacity.push(a.capacity[k]);
            called.push(a.called.get(k).copied().unwrap_or(0.0));
            paid.push(a.paid.get(k).copied().unwrap_or(0.0));
            call.push(a.call[k]);
            value.push(a.value[k]);
        }
    }
    DataFrame::new_infer_height(vec![
        Series::new("agreement".into(), name).into_column(),
        datetime("time", time.into_iter())?.into_column(),
        Series::new("balance".into(), balance).into_column(),
        Series::new("capacity".into(), capacity).into_column(),
        Series::new("called".into(), called).into_column(),
        Series::new("paid".into(), paid).into_column(),
        Series::new("call".into(), call).into_column(),
        Series::new("value".into(), value).into_column(),
    ])
}

/// A run's orders as a frame: one row per order, with how it ended.
///
/// # Errors
/// From Polars.
pub fn orders_frame(out: &LedgerOutput, entities: &Entities) -> PolarsResult<DataFrame> {
    let o = &out.order_log;
    DataFrame::new_infer_height(vec![
        Series::new("id".into(), o.iter().map(|r| r.id).collect::<Vec<u64>>()).into_column(),
        Series::new("entity".into(), o.iter().map(|r| entities.name(r.entity).to_owned()).collect::<Vec<String>>()).into_column(),
        Series::new("policy".into(), o.iter().map(|r| (r.policy != usize::MAX).then_some(r.policy as u64)).collect::<Vec<Option<u64>>>()).into_column(),
        datetime("submitted", o.iter().map(|r| r.submitted.0))?.into_column(),
        Series::new("ended".into(), o.iter().map(|r| r.ended.map(|t| t.0)).collect::<Vec<Option<i64>>>())
            .cast(&DataType::Datetime(TimeUnit::Nanoseconds, None))?
            .into_column(),
        Series::new("requested".into(), o.iter().map(|r| r.requested).collect::<Vec<f64>>()).into_column(),
        Series::new("filled".into(), o.iter().map(|r| r.filled).collect::<Vec<f64>>()).into_column(),
        Series::new("average_price".into(), o.iter().map(|r| (!r.average_price.is_nan()).then_some(r.average_price)).collect::<Vec<Option<f64>>>())
            .into_column(),
        Series::new("decision_value".into(), o.iter().map(|r| r.decision_value).collect::<Vec<f64>>()).into_column(),
        Series::new("attempts".into(), o.iter().map(|r| u64::from(r.attempts)).collect::<Vec<u64>>()).into_column(),
        Series::new("status".into(), o.iter().map(|r| format!("{:?}", r.status).to_lowercase()).collect::<Vec<String>>()).into_column(),
    ])
}

/// The holdings at every time, as a long frame (when the run kept them).
///
/// # Errors
/// From Polars.
pub fn holdings_frame(out: &LedgerOutput, entities: &Entities) -> PolarsResult<DataFrame> {
    let mut t = Vec::new();
    let mut e = Vec::new();
    let mut q = Vec::new();
    for (k, h) in out.holdings_path.iter().enumerate() {
        for (x, qty) in h {
            t.push(out.times[k].0);
            e.push(entities.name(*x).to_owned());
            q.push(*qty);
        }
    }
    DataFrame::new_infer_height(vec![
        datetime("time", t.into_iter())?.into_column(),
        Series::new("entity".into(), e).into_column(),
        Series::new("qty".into(), q).into_column(),
    ])
}

/// Unit records as a long frame: one row per contract and calendar time, with the unit's value,
/// cash, P&L terms, knowledge time and valuation state.
///
/// # Errors
/// From Polars.
pub fn records_frame(rec: &UnitRecords, entities: &Entities) -> PolarsResult<DataFrame> {
    let names: Vec<&str> = rec.entities().iter().map(|e| entities.name(*e)).collect();
    records_frame_named(rec, &names)
}

/// [`records_frame`] with the contracts' names given, in row order.
///
/// # Errors
/// From Polars, or if `names` is not one per contract.
pub fn records_frame_named(rec: &UnitRecords, names: &[&str]) -> PolarsResult<DataFrame> {
    if names.len() != rec.entities().len() {
        return Err(PolarsError::ShapeMismatch(format!("{} names for {} contracts", names.len(), rec.entities().len()).into()));
    }
    let times = rec.calendar().times();
    let t = times.len();
    let names: Vec<&str> = names.iter().flat_map(|n| std::iter::repeat_n(*n, t)).collect();
    let f = |name: &str, v: &[f64]| Series::new(name.into(), v.to_vec()).into_column();
    DataFrame::new_infer_height(vec![
        Series::new("entity".into(), names).into_column(),
        datetime("time", rec.entities().iter().flat_map(|_| times.iter().map(|x| x.0)))?.into_column(),
        f("value", &rec.value),
        f("cash", &rec.cash),
        f("income", &rec.income),
        f("price", &rec.price),
        f("principal", &rec.principal),
        f("other", &rec.other),
        f("financing", &rec.financing),
        datetime("known", rec.known.iter().copied())?.into_column(),
        Series::new("state".into(), rec.state.iter().map(|s| u32::from(*s)).collect::<Vec<u32>>()).into_column(),
    ])
}

/// The names behind a relationship or category column, when every value is an interned entity
/// (of kind `"category"` for categories): names appear only at the edges.
fn labels<'a>(c: &KColumn, entities: &'a Entities) -> Option<Vec<Option<&'a str>>> {
    let want = match c.unit {
        Unit::Ref => None,
        Unit::Category => Some("category"),
        _ => return None,
    };
    c.values
        .iter()
        .map(|x| {
            if x.is_nan() {
                return Some(None);
            }
            let e = EntityId::from_value(*x).filter(|e| (e.0 as usize) < entities.len())?;
            want.is_none_or(|k| entities.kind(e) == k).then(|| Some(entities.name(e)))
        })
        .collect()
}

/// A series as a long frame: entity, event time, knowledge time, values. Relationships and
/// categories come back as names.
///
/// # Errors
/// From Polars.
pub fn series_frame(s: &KSeries, entities: &Entities) -> PolarsResult<DataFrame> {
    let (ent, ev, kn): (Vec<EntityId>, Vec<Ts>, Vec<Ts>) = match s.layout() {
        fi_kernel::Layout::Rows { entity, event, known, .. } => (entity.clone(), event.clone(), known.clone()),
        fi_kernel::Layout::Grid { entities: es, times, known, .. } => {
            let n = es.len();
            let mut e = Vec::with_capacity(times.len() * n);
            let mut t = Vec::with_capacity(times.len() * n);
            let mut k = Vec::with_capacity(times.len() * n);
            for (ti, tt) in times.iter().enumerate() {
                for x in es {
                    e.push(*x);
                    t.push(*tt);
                    k.push(known[ti]);
                }
            }
            (e, t, k)
        }
    };
    let mut cols = vec![
        Series::new("entity".into(), ent.iter().map(|e| entities.name(*e).to_owned()).collect::<Vec<String>>()).into_column(),
        datetime("time", ev.iter().map(|t| t.0))?.into_column(),
        datetime("known", kn.iter().map(|t| t.0))?.into_column(),
    ];
    for c in s.columns() {
        if let Some(names) = labels(c, entities) {
            cols.push(Series::new(c.name.as_str().into(), names).into_column());
            continue;
        }
        let v: Vec<Option<f64>> = c.values.iter().map(|x| (!x.is_nan()).then_some(*x)).collect();
        cols.push(Series::new(c.name.as_str().into(), v).into_column());
    }
    DataFrame::new_infer_height(cols)
}
