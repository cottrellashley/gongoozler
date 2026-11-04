//! Debt from its terms, from Python: business calendars, coupons, bonds, curves and their
//! analytics (`fi-terms`), and the debt contracts a workspace steps (`fi_std::DebtContract`).
//!
//! Amounts follow the engine's unit, one unit of original face: a bond's cash flows and accrued
//! interest are per unit of original face. Prices follow the market's quotation, per 100 of current
//! face.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use polars::prelude::{Column, DataFrame, DataType, IntoColumn, NamedFrom, Series};
use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyTuple};

use fi_kernel::{Contract, EntityId, InCurrency, Ts, Workspace};
use fi_std::{Compounding, DebtContract, Spread, Valuation};
use fi_terms::calendar::{Observance, Rule};
use fi_terms::curve::Projected;
use fi_terms::{
    Amortisation, Bump, BumpKind, BusinessCalendar, BusinessDay, Cashflow, CashflowKind, Coupon, Curve, Date, DayCount, Debt, Extension, Fixings,
    Interpolation, Method, Price, Right, RightKind, Schedule, Stub, YieldConvention, analytics,
};

use crate::{EngineError, InvalidError, MissingError, convert, split};

/// A terms error as a Python exception.
fn bad(e: fi_terms::Error) -> PyErr {
    match e {
        fi_terms::Error::Invalid(m) => InvalidError::new_err(m),
        fi_terms::Error::Missing(m) => MissingError::new_err(m),
        fi_terms::Error::NoSolution(m) => EngineError::new_err(m),
    }
}

/// A date from Python: a `datetime.date`, a `datetime` (its UTC day), an ISO string, a pandas or
/// NumPy time, or nanoseconds.
pub(crate) fn day(obj: &Bound<'_, PyAny>) -> PyResult<Date> {
    Ok(Date(i32::try_from(convert::ts(obj)?.days()).map_err(|_| PyValueError::new_err("a date out of range"))?))
}

fn py_date(py: Python<'_>, d: Date) -> PyResult<Bound<'_, PyAny>> {
    convert::date(py, Ts::from_days(i64::from(d.0)))
}

fn date_column(name: &str, days: Vec<Option<i32>>) -> PyResult<Column> {
    Ok(Series::new(name.into(), days).cast(&DataType::Date).map_err(convert::err)?.into_column())
}

fn parse<T: std::str::FromStr<Err = fi_terms::Error>>(s: &str) -> PyResult<T> {
    s.parse().map_err(bad)
}

fn convention(s: &str) -> PyResult<YieldConvention> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "street" => YieldConvention::Street,
        "annual" => YieldConvention::Annual,
        "continuous" => YieldConvention::Continuous,
        _ => return Err(PyValueError::new_err(format!("no yield convention {s:?} (street, annual, continuous)"))),
    })
}

fn interpolation(s: &str) -> PyResult<Interpolation> {
    Ok(match s.to_ascii_lowercase().replace('-', "_").as_str() {
        "linear_zero" => Interpolation::LinearZero,
        "log_linear_discount" => Interpolation::LogLinearDiscount,
        _ => return Err(PyValueError::new_err(format!("no interpolation {s:?} (linear_zero, log_linear_discount)"))),
    })
}

fn compounding(s: &str) -> PyResult<Compounding> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "continuous" => Compounding::Continuous,
        "annual" => Compounding::Annual,
        _ => return Err(PyValueError::new_err(format!("no compounding {s:?} (continuous, annual)"))),
    })
}

// ------------------------------------------------------------------------------------ calendars

/// Business days: a weekend and declared holidays. Nothing here knows a country: holidays are
/// what the user declares, as dates or as rules.
#[pyclass(name = "BusinessCalendar", module = "fi_engine._native", frozen)]
#[derive(Clone)]
pub struct PyBusinessCalendar {
    pub(crate) cal: BusinessCalendar,
}

fn rule(r: &Bound<'_, PyAny>) -> PyResult<Rule> {
    let t = r.cast::<PyTuple>().map_err(|_| {
        PyTypeError::new_err("a holiday rule is a tuple: (\"fixed\", month, day[, observance]), (\"nth_weekday\", month, weekday, n) or (\"easter\", offset)")
    })?;
    let kind: String = t.get_item(0)?.extract()?;
    Ok(match kind.as_str() {
        "fixed" => {
            let observance = match t.len() {
                3 => Observance::Actual,
                _ => match t.get_item(3)?.extract::<String>()?.as_str() {
                    "actual" => Observance::Actual,
                    "nearest" => Observance::Nearest,
                    "next_weekday" => Observance::NextWeekday,
                    o => return Err(PyValueError::new_err(format!("no observance {o:?} (actual, nearest, next_weekday)"))),
                },
            };
            Rule::Fixed { month: t.get_item(1)?.extract()?, day: t.get_item(2)?.extract()?, observance }
        }
        "nth_weekday" => Rule::NthWeekday { month: t.get_item(1)?.extract()?, weekday: t.get_item(2)?.extract()?, n: t.get_item(3)?.extract()? },
        "easter" => Rule::Easter { offset: t.get_item(1)?.extract()? },
        k => return Err(PyValueError::new_err(format!("no holiday rule {k:?} (fixed, nth_weekday, easter)"))),
    })
}

#[pymethods]
impl PyBusinessCalendar {
    /// A calendar of `holidays` (dates) and `weekend` days (0 is Monday; default Saturday and
    /// Sunday).
    #[new]
    #[pyo3(signature = (name, holidays = None, *, weekend = None))]
    fn new(name: &str, holidays: Option<Vec<Bound<'_, PyAny>>>, weekend: Option<Vec<u32>>) -> PyResult<Self> {
        let days = holidays.unwrap_or_default().iter().map(day).collect::<PyResult<Vec<_>>>()?;
        Ok(Self { cal: BusinessCalendar::new(name, &weekend.unwrap_or_else(|| vec![5, 6]), days).map_err(bad)? })
    }

    /// A calendar of the holidays `rules` make in `years` (first, last, inclusive), plus `extra`
    /// dates. A rule is `("fixed", month, day)` or `("fixed", month, day, observance)` with
    /// observance `"actual"`, `"nearest"` (Saturday's on Friday, Sunday's on Monday) or
    /// `"next_weekday"`; `("nth_weekday", month, weekday, n)` (weekday 0 is Monday, n = -1 the
    /// last); or `("easter", offset)` (days from Easter Sunday: -2 Good Friday, 1 Easter Monday).
    #[staticmethod]
    #[pyo3(signature = (name, rules, years, *, weekend = None, extra = None))]
    fn from_rules(
        name: &str,
        rules: Vec<Bound<'_, PyAny>>,
        years: (i32, i32),
        weekend: Option<Vec<u32>>,
        extra: Option<Vec<Bound<'_, PyAny>>>,
    ) -> PyResult<Self> {
        let rules = rules.iter().map(rule).collect::<PyResult<Vec<_>>>()?;
        let extra = extra.unwrap_or_default().iter().map(day).collect::<PyResult<Vec<_>>>()?;
        Ok(Self { cal: BusinessCalendar::from_rules(name, &weekend.unwrap_or_else(|| vec![5, 6]), &rules, years, extra).map_err(bad)? })
    }

    /// Saturdays and Sundays, and no holidays.
    #[staticmethod]
    fn weekends() -> Self {
        Self { cal: BusinessCalendar::weekends() }
    }

    /// The days that are business days on every one of `calendars`.
    #[staticmethod]
    fn joint(calendars: Vec<PyRef<'_, Self>>) -> Self {
        Self { cal: BusinessCalendar::joint(&calendars.iter().map(|c| &c.cal).collect::<Vec<_>>()) }
    }

    #[getter]
    fn name(&self) -> String {
        self.cal.name.clone()
    }

    /// The holidays declared, as dates.
    fn holidays<'py>(&self, py: Python<'py>) -> PyResult<Vec<Bound<'py, PyAny>>> {
        self.cal.holidays().iter().map(|d| py_date(py, *d)).collect()
    }

    fn is_business_day(&self, date: &Bound<'_, PyAny>) -> PyResult<bool> {
        Ok(self.cal.is_business_day(day(date)?))
    }

    /// `date` moved by `convention`: `"unadjusted"`, `"following"`, `"modified_following"`,
    /// `"preceding"` or `"modified_preceding"`.
    #[pyo3(signature = (date, convention = "following"))]
    fn adjust<'py>(&self, py: Python<'py>, date: &Bound<'_, PyAny>, convention: &str) -> PyResult<Bound<'py, PyAny>> {
        py_date(py, self.cal.adjust(day(date)?, parse::<BusinessDay>(convention)?))
    }

    /// `n` business days after `date` (before it when negative).
    fn add_business_days<'py>(&self, py: Python<'py>, date: &Bound<'_, PyAny>, n: i32) -> PyResult<Bound<'py, PyAny>> {
        py_date(py, self.cal.add_business_days(day(date)?, n))
    }

    /// Business days after `start` up to and including `end`.
    fn business_days_between(&self, start: &Bound<'_, PyAny>, end: &Bound<'_, PyAny>) -> PyResult<i32> {
        Ok(self.cal.business_days_between(day(start)?, day(end)?))
    }

    fn __repr__(&self) -> String {
        format!("BusinessCalendar({:?}, {} holidays)", self.cal.name, self.cal.holidays().len())
    }
}

// -------------------------------------------------------------------------------------- coupons

/// How each period's coupon rate is set.
#[pyclass(name = "Coupon", module = "fi_engine._native", frozen)]
#[derive(Clone)]
pub struct PyCoupon {
    pub(crate) coupon: Coupon,
}

#[pymethods]
impl PyCoupon {
    /// A fixed rate a year, as a decimal (0.05 for 5%).
    #[staticmethod]
    fn fixed(rate: f64) -> Self {
        Self { coupon: Coupon::Fixed { rate } }
    }

    /// A stepped rate: `(date, rate)` pairs, each rate for the periods starting on or after its
    /// date.
    #[staticmethod]
    fn step(rates: Vec<(Bound<'_, PyAny>, f64)>) -> PyResult<Self> {
        let mut rates = rates.iter().map(|(d, r)| Ok((day(d)?, *r))).collect::<PyResult<Vec<_>>>()?;
        rates.sort_by_key(|(d, _)| *d);
        Ok(Self { coupon: Coupon::Step { rates } })
    }

    /// `gearing × index + spread`, capped and floored, on fixings of `index` (decimals). `method`:
    /// `"in_advance"` (one fixing `lag` business days before the period starts: a term rate),
    /// `"compounded"` or `"average"` (daily fixings in arrears, each observed `lookback` business
    /// days earlier, the last `lockout` days at the rate fixed when the lockout starts: an
    /// overnight rate). Business days are `calendar`'s (default: weekends only).
    #[staticmethod]
    #[pyo3(signature = (index, *, spread = 0.0, gearing = 1.0, cap = None, floor = None, method = "in_advance", lag = 2, lookback = 0, lockout = 0, calendar = None))]
    #[allow(clippy::too_many_arguments)]
    fn floating(
        index: &str,
        spread: f64,
        gearing: f64,
        cap: Option<f64>,
        floor: Option<f64>,
        method: &str,
        lag: i32,
        lookback: i32,
        lockout: i32,
        calendar: Option<PyRef<'_, PyBusinessCalendar>>,
    ) -> PyResult<Self> {
        let method = match method {
            "in_advance" => Method::InAdvance { lag },
            "compounded" => Method::CompoundedInArrears { lookback, lockout },
            "average" => Method::AverageInArrears { lookback, lockout },
            m => return Err(PyValueError::new_err(format!("no floating method {m:?} (in_advance, compounded, average)"))),
        };
        let calendar = calendar.map_or_else(BusinessCalendar::weekends, |c| c.cal.clone());
        Ok(Self { coupon: Coupon::Floating { index: index.into(), spread, gearing, cap, floor, method, calendar } })
    }

    /// A real rate on principal indexed to `index` (monthly levels, each keyed by the first day of
    /// its month), from the `base` level, `lag` months behind, interpolated daily between months
    /// if `interpolate`. With `floor`, principal repays at no less than par.
    #[staticmethod]
    #[pyo3(signature = (index, real, base, *, lag = 3, interpolate = true, floor = true))]
    fn inflation(index: &str, real: f64, base: f64, lag: u32, interpolate: bool, floor: bool) -> Self {
        Self { coupon: Coupon::Inflation { index: index.into(), real, base, lag, interpolate, floor } }
    }

    /// No coupon.
    #[staticmethod]
    fn zero() -> Self {
        Self { coupon: Coupon::Zero }
    }

    fn __repr__(&self) -> String {
        format!("Coupon({:?})", self.coupon)
    }
}

// -------------------------------------------------------------------------------------- fixings

/// Fixings from Python: `{index: {date: value}}` or `{index: [(date, value), ...]}`.
pub(crate) struct MapFixings(HashMap<String, BTreeMap<Date, f64>>);

impl Fixings for MapFixings {
    fn fixing(&self, index: &str, date: Date) -> Option<f64> {
        self.0.get(index)?.get(&date).copied()
    }
}

fn fixings(obj: Option<&Bound<'_, PyAny>>) -> PyResult<MapFixings> {
    let mut out = HashMap::new();
    let Some(obj) = obj else { return Ok(MapFixings(out)) };
    let d = obj.cast::<PyDict>().map_err(|_| PyTypeError::new_err("fixings are a dict: {index: {date: value}} or {index: [(date, value), ...]}"))?;
    for (k, v) in d.iter() {
        let mut m = BTreeMap::new();
        if let Ok(inner) = v.cast::<PyDict>() {
            for (dt, x) in inner.iter() {
                m.insert(day(&dt)?, x.extract::<f64>()?);
            }
        } else {
            for pair in v.try_iter()? {
                let (dt, x): (Bound<'_, PyAny>, f64) = pair?.extract()?;
                m.insert(day(&dt)?, x);
            }
        }
        out.insert(k.extract::<String>()?, m);
    }
    Ok(MapFixings(out))
}

/// The fixings to use: the observed ones, and (if a forecast curve is given) forwards from it for
/// the coupon's index where none was observed.
fn with_forecast<'a>(debt: &Debt, observed: &'a MapFixings, forecast: Option<&'a Curve>) -> Option<Projected<'a>> {
    Projected::for_debt(debt, observed, forecast?)
}

// ---------------------------------------------------------------------------------------- bonds

/// A debt instrument from its terms: a coupon leg on a schedule, principal (bullet or
/// amortising), calls and puts, and a soft-bullet extension.
#[pyclass(name = "Bond", module = "fi_engine._native", frozen)]
pub struct PyBond {
    pub(crate) debt: Arc<Debt>,
    /// The currency the book holds it in (`None`: the book's base).
    pub(crate) currency: Option<String>,
}

fn rights(kind: RightKind, items: Option<Vec<Bound<'_, PyAny>>>) -> PyResult<Vec<Right>> {
    items
        .unwrap_or_default()
        .iter()
        .map(|r| {
            let t = r.cast::<PyTuple>().map_err(|_| PyTypeError::new_err("a call or put is (from, to, price[, notice[, make_whole_bp]])"))?;
            if !(3..=5).contains(&t.len()) {
                return Err(PyValueError::new_err("a call or put is (from, to, price[, notice[, make_whole_bp]])"));
            }
            Ok(Right {
                kind,
                from: day(&t.get_item(0)?)?,
                to: day(&t.get_item(1)?)?,
                price: t.get_item(2)?.extract()?,
                notice: if t.len() > 3 { t.get_item(3)?.extract()? } else { 0 },
                make_whole_bp: if t.len() > 4 { t.get_item(4)?.extract()? } else { None },
            })
        })
        .collect()
}

fn coupon_of(obj: Option<&Bound<'_, PyAny>>) -> PyResult<Coupon> {
    let Some(o) = obj else { return Ok(Coupon::Zero) };
    if o.is_none() {
        return Ok(Coupon::Zero);
    }
    if let Ok(c) = o.cast::<PyCoupon>() {
        return Ok(c.get().coupon.clone());
    }
    o.extract::<f64>().map(|rate| Coupon::Fixed { rate }).map_err(|_| PyTypeError::new_err("a coupon is a rate (0.05), a Coupon, or None for a zero"))
}

fn cashflow_frame(flows: &[Cashflow]) -> PyResult<DataFrame> {
    let kind: Vec<&str> = flows
        .iter()
        .map(|c| match c.kind {
            CashflowKind::Coupon => "coupon",
            CashflowKind::Principal => "principal",
            CashflowKind::Redemption => "redemption",
        })
        .collect();
    let cols = vec![
        Series::new("kind".into(), kind).into_column(),
        date_column("date", flows.iter().map(|c| Some(c.date.0)).collect())?,
        date_column("pay", flows.iter().map(|c| Some(c.pay.0)).collect())?,
        date_column("ex", flows.iter().map(|c| c.ex.map(|d| d.0)).collect())?,
        date_column("accrual_start", flows.iter().map(|c| c.accrual.map(|a| a.0.0)).collect())?,
        date_column("accrual_end", flows.iter().map(|c| c.accrual.map(|a| a.1.0)).collect())?,
        Series::new("fraction".into(), flows.iter().map(|c| c.fraction).collect::<Vec<_>>()).into_column(),
        Series::new("rate".into(), flows.iter().map(|c| c.rate).collect::<Vec<_>>()).into_column(),
        Series::new("amount".into(), flows.iter().map(|c| c.amount).collect::<Vec<_>>()).into_column(),
        Series::new("face".into(), flows.iter().map(|c| c.face).collect::<Vec<_>>()).into_column(),
    ];
    DataFrame::new_infer_height(cols).map_err(convert::err)
}

fn price_dict<'py>(py: Python<'py>, p: Price) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    d.set_item("clean", p.clean)?;
    d.set_item("accrued", p.accrued)?;
    d.set_item("dirty", p.dirty)?;
    Ok(d)
}

fn bump_of(obj: Option<&Bound<'_, PyAny>>, size_bp: f64) -> PyResult<Bump> {
    let kind = match obj {
        None => BumpKind::Parallel,
        Some(o) => {
            if let Ok(point) = o.extract::<usize>() {
                BumpKind::KeyRate { point }
            } else {
                match o.extract::<String>()?.as_str() {
                    "parallel" => BumpKind::Parallel,
                    "spread" => BumpKind::Spread,
                    b => return Err(PyValueError::new_err(format!("no bump {b:?} (\"parallel\", \"spread\", or a curve point's index for a key rate)"))),
                }
            }
        }
    };
    if !(size_bp.is_finite() && size_bp > 0.0) {
        return Err(PyValueError::new_err(format!("a bump of {size_bp}bp")));
    }
    Ok(Bump { kind, size_bp })
}

#[pymethods]
impl PyBond {
    /// A bond accruing from `issue` to `maturity`.
    ///
    /// * `coupon`: a rate (0.05), a `Coupon`, or `None` for a zero; `frequency` coupons a year (0
    ///   for one period); `day_count` (`"ACT/ACT ICMA"`, `"30/360"`, `"ACT/360"`, ...).
    /// * The schedule rolls back from maturity (`stub`: `"short_front"`, `"long_front"`,
    ///   `"short_back"`, `"long_back"`), or through `first` and `last` regular dates, or through
    ///   explicit `dates`; `eom` keeps month ends; payment dates move by `business_day` on
    ///   `calendar` (accrual dates too if `adjust_accrual`), `pay_lag` business days later.
    /// * A coupon goes ex `ex_days` business days before it pays: the holder then is paid it.
    /// * `redemption` per 100 at maturity; `amortisation`: `(date, face, price)` per unit of
    ///   original face retired at a price per 100 (a sinking fund).
    /// * `calls` and `puts`: `(from, to, price)`, optionally with `notice` business days and a
    ///   make-whole spread in basis points; `extension`: `(to,)` or `(to, coupon)` for a soft
    ///   bullet.
    /// * `settle_days`: business days from a trade to settlement; `currency`: what a book holds
    ///   it in (default: the book's base).
    /// * `rounding`: decimal places each contractual amount per unit of original face is rounded
    ///   to, half away from zero (5: cents per 1,000 of face); default unrounded.
    #[new]
    #[pyo3(signature = (
        issue, maturity, coupon = None, *, day_count, frequency = 2, currency = None, calendar = None, business_day = "following",
        adjust_accrual = false, eom = false, stub = "short_front", first = None, last = None, dates = None, pay_lag = 0, ex_days = 0,
        settle_days = 0, redemption = 100.0, amortisation = None, calls = None, puts = None, extension = None, rounding = None
    ))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        issue: &Bound<'_, PyAny>,
        maturity: &Bound<'_, PyAny>,
        coupon: Option<&Bound<'_, PyAny>>,
        day_count: &str,
        frequency: u32,
        currency: Option<String>,
        calendar: Option<PyRef<'_, PyBusinessCalendar>>,
        business_day: &str,
        adjust_accrual: bool,
        eom: bool,
        stub: &str,
        first: Option<&Bound<'_, PyAny>>,
        last: Option<&Bound<'_, PyAny>>,
        dates: Option<Vec<Bound<'_, PyAny>>>,
        pay_lag: i32,
        ex_days: i32,
        settle_days: i32,
        redemption: f64,
        amortisation: Option<Vec<(Bound<'_, PyAny>, f64, f64)>>,
        calls: Option<Vec<Bound<'_, PyAny>>>,
        puts: Option<Vec<Bound<'_, PyAny>>>,
        extension: Option<&Bound<'_, PyAny>>,
        rounding: Option<u32>,
    ) -> PyResult<Self> {
        if frequency != 0 && !12u32.is_multiple_of(frequency) {
            return Err(PyValueError::new_err(format!("{frequency} coupons a year do not divide twelve months")));
        }
        let mut schedule = Schedule::new(day(issue)?, day(maturity)?, 12u32.checked_div(frequency).unwrap_or(0));
        schedule.stub = match stub {
            "short_front" => Stub::ShortFront,
            "long_front" => Stub::LongFront,
            "short_back" => Stub::ShortBack,
            "long_back" => Stub::LongBack,
            s => return Err(PyValueError::new_err(format!("no stub {s:?} (short_front, long_front, short_back, long_back)"))),
        };
        schedule.first = first.map(day).transpose()?;
        schedule.last = last.map(day).transpose()?;
        schedule.eom = eom;
        if let Some(c) = calendar {
            schedule.calendar = c.cal.clone();
        }
        schedule.adjust = parse::<BusinessDay>(business_day)?;
        schedule.adjust_accrual = adjust_accrual;
        schedule.pay_lag = pay_lag;
        schedule.ex_days = ex_days;
        schedule.dates = dates.map(|ds| ds.iter().map(day).collect::<PyResult<Vec<_>>>()).transpose()?;
        let amortisation = match amortisation {
            None => Amortisation::Bullet,
            Some(a) => {
                let mut s = a.iter().map(|(d, f, p)| Ok((day(d)?, *f, *p))).collect::<PyResult<Vec<_>>>()?;
                s.sort_by_key(|(d, _, _)| *d);
                Amortisation::Schedule(s)
            }
        };
        let mut all = rights(RightKind::Call, calls)?;
        all.extend(rights(RightKind::Put, puts)?);
        let extension = match extension {
            None => None,
            Some(e) => {
                let t = e.cast::<PyTuple>().map_err(|_| PyTypeError::new_err("an extension is (to,) or (to, coupon)"))?;
                let coupon = if t.len() > 1 { Some(coupon_of(Some(&t.get_item(1)?))?) } else { None };
                Some(Extension { to: day(&t.get_item(0)?)?, coupon })
            }
        };
        let debt = Debt {
            currency: currency.clone().unwrap_or_default(),
            schedule,
            day_count: parse::<DayCount>(day_count)?,
            coupon: coupon_of(coupon)?,
            redemption,
            amortisation,
            rights: all,
            extension,
            settle_days,
            rounding,
        };
        debt.validate().map_err(bad)?;
        Ok(Self { debt: Arc::new(debt), currency })
    }

    #[getter]
    fn issue<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        py_date(py, self.debt.issue())
    }

    #[getter]
    fn maturity<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        py_date(py, self.debt.maturity())
    }

    #[getter]
    fn currency(&self) -> Option<String> {
        self.currency.clone()
    }

    #[getter]
    fn day_count(&self) -> &'static str {
        self.debt.day_count.name()
    }

    #[getter]
    fn frequency(&self) -> u32 {
        self.debt.schedule.per_year()
    }

    /// The coupon periods: accrual start and end, payment and ex dates, and whether each is regular.
    fn periods<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let ps = self.debt.periods().map_err(bad)?;
        let df = DataFrame::new_infer_height(vec![
            date_column("start", ps.iter().map(|p| Some(p.start.0)).collect())?,
            date_column("end", ps.iter().map(|p| Some(p.end.0)).collect())?,
            date_column("pay", ps.iter().map(|p| Some(p.pay.0)).collect())?,
            date_column("ex", ps.iter().map(|p| p.ex.map(|d| d.0)).collect())?,
            Series::new("regular".into(), ps.iter().map(|p| p.regular).collect::<Vec<_>>()).into_column(),
        ])
        .map_err(convert::err)?;
        convert::to_python(py, df)
    }

    /// Every contractual flow, per unit of original face: kind, scheduled and payment dates, the
    /// ex date and accrual period of a coupon, its year fraction and rate, the amount, and the face
    /// a principal flow retires. A floater needs `fixings` (`{index: {date: rate}}`) for each
    /// period, or a `forecast` curve to project the ones not observed.
    #[pyo3(signature = (fixings = None, *, forecast = None))]
    fn cashflows<'py>(&self, py: Python<'py>, fixings: Option<&Bound<'_, PyAny>>, forecast: Option<PyRef<'_, PyCurve>>) -> PyResult<Bound<'py, PyAny>> {
        let fx = self::fixings(fixings)?;
        let projected = with_forecast(&self.debt, &fx, forecast.as_ref().map(|c| &c.curve));
        let f: &dyn Fixings = match &projected {
            Some(p) => p,
            None => &fx,
        };
        convert::to_python(py, cashflow_frame(&self.debt.cashflows(f).map_err(bad)?)?)
    }

    /// Face outstanding per unit of original face at the end of `date`.
    fn balance(&self, date: &Bound<'_, PyAny>) -> PyResult<f64> {
        Ok(self.debt.balance(day(date)?))
    }

    /// Accrued interest on `date`, per unit of original face (negative once a coupon has gone ex).
    #[pyo3(signature = (date, fixings = None))]
    fn accrued(&self, date: &Bound<'_, PyAny>, fixings: Option<&Bound<'_, PyAny>>) -> PyResult<f64> {
        self.debt.accrued(day(date)?, &self::fixings(fixings)?).map_err(bad)
    }

    /// The settlement date of a trade on `trade`.
    fn settlement<'py>(&self, py: Python<'py>, trade: &Bound<'_, PyAny>) -> PyResult<Bound<'py, PyAny>> {
        py_date(py, self.debt.settlement(day(trade)?))
    }

    /// The price per 100 of current face (clean, accrued, dirty) for settlement on `settle`: from
    /// a `yield_` (`convention`: `"street"`, compounding at the coupon frequency; `"annual"`;
    /// `"continuous"`), or from a discount `curve` plus a continuously compounded `spread`.
    #[pyo3(signature = (settle, *, yield_ = None, curve = None, spread = 0.0, convention = "street", fixings = None, forecast = None))]
    #[allow(clippy::too_many_arguments)]
    fn price<'py>(
        &self,
        py: Python<'py>,
        settle: &Bound<'_, PyAny>,
        yield_: Option<f64>,
        curve: Option<PyRef<'_, PyCurve>>,
        spread: f64,
        convention: &str,
        fixings: Option<&Bound<'_, PyAny>>,
        forecast: Option<PyRef<'_, PyCurve>>,
    ) -> PyResult<Bound<'py, PyDict>> {
        let settle = day(settle)?;
        let fx = self::fixings(fixings)?;
        let projected = with_forecast(&self.debt, &fx, forecast.as_ref().map(|c| &c.curve));
        let f: &dyn Fixings = match &projected {
            Some(p) => p,
            None => &fx,
        };
        let p = match (yield_, curve) {
            (Some(y), None) => analytics::price_from_yield(&self.debt, settle, y, self::convention(convention)?, f),
            (None, Some(c)) => analytics::price_from_curve(&self.debt, settle, &c.curve, spread, f),
            _ => return Err(PyValueError::new_err("a price comes from a yield_ or a curve: give one")),
        }
        .map_err(bad)?;
        price_dict(py, p)
    }

    /// The yield (a decimal) whose clean price for settlement on `settle` is `clean`.
    #[pyo3(name = "yield_", signature = (settle, clean, *, convention = "street", fixings = None))]
    fn yield_from_price(&self, settle: &Bound<'_, PyAny>, clean: f64, convention: &str, fixings: Option<&Bound<'_, PyAny>>) -> PyResult<f64> {
        analytics::yield_from_price(&self.debt, day(settle)?, clean, self::convention(convention)?, &self::fixings(fixings)?).map_err(bad)
    }

    /// The Z-spread: the continuously compounded spread over `curve` whose clean price is `clean`.
    #[pyo3(signature = (settle, clean, curve, *, fixings = None, forecast = None))]
    fn z_spread(
        &self,
        settle: &Bound<'_, PyAny>,
        clean: f64,
        curve: PyRef<'_, PyCurve>,
        fixings: Option<&Bound<'_, PyAny>>,
        forecast: Option<PyRef<'_, PyCurve>>,
    ) -> PyResult<f64> {
        let fx = self::fixings(fixings)?;
        let projected = with_forecast(&self.debt, &fx, forecast.as_ref().map(|c| &c.curve));
        let f: &dyn Fixings = match &projected {
            Some(p) => p,
            None => &fx,
        };
        analytics::z_spread(&self.debt, day(settle)?, clean, &curve.curve, f).map_err(bad)
    }

    /// Macaulay duration (years), modified duration and convexity at a yield.
    #[pyo3(signature = (settle, yield_, *, convention = "street", fixings = None))]
    fn durations<'py>(
        &self,
        py: Python<'py>,
        settle: &Bound<'_, PyAny>,
        yield_: f64,
        convention: &str,
        fixings: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Bound<'py, PyDict>> {
        let (mac, modified, convexity) =
            analytics::durations(&self.debt, day(settle)?, yield_, self::convention(convention)?, &self::fixings(fixings)?).map_err(bad)?;
        let d = PyDict::new(py);
        d.set_item("macaulay", mac)?;
        d.set_item("modified", modified)?;
        d.set_item("convexity", convexity)?;
        Ok(d)
    }

    /// A sensitivity of the dirty price per 100 to a bump of `curve` or the `spread` over it, by
    /// central difference of `size_bp` either way: `bump="parallel"` (every zero rate),
    /// `"spread"`, or a curve point's index (a key rate). Returns the value per 1bp fall and the
    /// definition of what was measured, which a risk number should never travel without.
    #[pyo3(signature = (settle, curve, *, spread = 0.0, bump = None, size_bp = 1.0, fixings = None, forecast = None))]
    #[allow(clippy::too_many_arguments)]
    fn risk<'py>(
        &self,
        py: Python<'py>,
        settle: &Bound<'_, PyAny>,
        curve: PyRef<'_, PyCurve>,
        spread: f64,
        bump: Option<&Bound<'_, PyAny>>,
        size_bp: f64,
        fixings: Option<&Bound<'_, PyAny>>,
        forecast: Option<PyRef<'_, PyCurve>>,
    ) -> PyResult<Bound<'py, PyDict>> {
        let fx = self::fixings(fixings)?;
        let projected = with_forecast(&self.debt, &fx, forecast.as_ref().map(|c| &c.curve));
        let f: &dyn Fixings = match &projected {
            Some(p) => p,
            None => &fx,
        };
        let s = analytics::sensitivity(&self.debt, day(settle)?, &curve.curve, spread, bump_of(bump, size_bp)?, f).map_err(bad)?;
        let d = PyDict::new(py);
        d.set_item("value", s.value)?;
        d.set_item("definition", s.definition)?;
        d.set_item("size_bp", s.bump.size_bp)?;
        Ok(d)
    }

    fn __repr__(&self) -> String {
        let t = &self.debt;
        let coupon = match &t.coupon {
            Coupon::Fixed { rate } => format!("{}%", rate * 100.0),
            Coupon::Step { .. } => "stepped".into(),
            Coupon::Floating { index, spread, .. } => format!("{index} + {}bp", spread * 1e4),
            Coupon::Inflation { index, real, .. } => format!("{}% real on {index}", real * 100.0),
            Coupon::Zero => "zero".into(),
        };
        format!("Bond({coupon}, {} to {}, {}x a year, {})", t.issue(), t.maturity(), t.schedule.per_year(), t.day_count)
    }
}

// --------------------------------------------------------------------------------------- curves

/// A discount curve: zero rates at times in years (ACT/365F from `reference`), interpolated as
/// declared.
#[pyclass(name = "Curve", module = "fi_engine._native", frozen)]
pub struct PyCurve {
    pub(crate) curve: Curve,
}

#[pymethods]
impl PyCurve {
    /// A curve through `zeros` at `times` (years), compounded `compounding` (`"continuous"` or
    /// `"annual"`), interpolated `interpolation` (`"linear_zero"` or `"log_linear_discount"`).
    #[new]
    #[pyo3(signature = (reference, times, zeros, *, compounding = "continuous", interpolation = "linear_zero"))]
    fn new(reference: &Bound<'_, PyAny>, times: Vec<f64>, zeros: Vec<f64>, compounding: &str, interpolation: &str) -> PyResult<Self> {
        let zeros = match self::compounding(compounding)? {
            Compounding::Continuous => zeros,
            Compounding::Annual => zeros.iter().map(|z| z.ln_1p()).collect(),
        };
        Ok(Self { curve: Curve::new(day(reference)?, times, zeros, self::interpolation(interpolation)?).map_err(bad)? })
    }

    /// A curve through discount factors at `times` (years).
    #[staticmethod]
    #[pyo3(signature = (reference, times, discount_factors, *, interpolation = "log_linear_discount"))]
    fn from_discount_factors(reference: &Bound<'_, PyAny>, times: Vec<f64>, discount_factors: Vec<f64>, interpolation: &str) -> PyResult<Self> {
        Ok(Self { curve: Curve::from_discount_factors(day(reference)?, times, &discount_factors, self::interpolation(interpolation)?).map_err(bad)? })
    }

    /// A flat curve at a continuously compounded `rate`.
    #[staticmethod]
    fn flat(reference: &Bound<'_, PyAny>, rate: f64) -> PyResult<Self> {
        Ok(Self { curve: Curve::flat(day(reference)?, rate) })
    }

    #[getter]
    fn reference<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        py_date(py, self.curve.reference)
    }

    #[getter]
    fn times(&self) -> Vec<f64> {
        self.curve.times.clone()
    }

    /// The continuously compounded zero rate at `t` years.
    fn zero(&self, t: f64) -> f64 {
        self.curve.zero(t)
    }

    /// The discount factor at `t` years, or to a date.
    fn discount(&self, t: &Bound<'_, PyAny>) -> PyResult<f64> {
        if let Ok(x) = t.extract::<f64>()
            && !t.is_instance_of::<pyo3::types::PyInt>()
        {
            return Ok(self.curve.discount(x));
        }
        Ok(self.curve.discount_to(day(t)?))
    }

    /// The simple forward rate from `start` to `end` in `day_count`.
    #[pyo3(signature = (start, end, day_count = "ACT/360"))]
    fn forward(&self, start: &Bound<'_, PyAny>, end: &Bound<'_, PyAny>, day_count: &str) -> PyResult<f64> {
        Ok(self.curve.forward(day(start)?, day(end)?, parse::<DayCount>(day_count)?))
    }

    /// The curve with every zero rate moved by `bp` basis points.
    fn shifted(&self, bp: f64) -> Self {
        Self { curve: self.curve.shifted(bp) }
    }

    /// The curve with one point moved by `bp` basis points (a key-rate bump).
    fn point_shifted(&self, point: usize, bp: f64) -> PyResult<Self> {
        Ok(Self { curve: self.curve.point_shifted(point, bp).map_err(bad)? })
    }

    fn __repr__(&self) -> String {
        format!("Curve({}, {} points)", self.curve.reference, self.curve.times.len())
    }
}

// ------------------------------------------------------------------------------- debt contracts

/// A valuation from Python: `("clean", "series.column")`, `("yield", "series.column",
/// convention)` or `("curve", series, curve, [(column, years), ...], compounding, interpolation,
/// spread)` with spread a number or `"series.column"`.
fn valuation(ws: &Workspace, v: &Bound<'_, PyAny>) -> PyResult<Valuation> {
    let t = v.cast::<PyTuple>().map_err(|_| PyTypeError::new_err("a valuation is fi.valuation.clean(...), .yield_(...) or .curve(...)"))?;
    let kind: String = t.get_item(0)?.extract()?;
    let col = |spec: &str| -> PyResult<(String, String)> {
        let (s, c) = split(spec)?;
        ws.series(s).and_then(|x| x.column(c)).map_err(crate::to_py)?;
        Ok((s.to_owned(), c.to_owned()))
    };
    Ok(match kind.as_str() {
        "clean" => {
            let (series, column) = col(&t.get_item(1)?.extract::<String>()?)?;
            Valuation::Clean { series, column }
        }
        "yield" => {
            let (series, column) = col(&t.get_item(1)?.extract::<String>()?)?;
            Valuation::Yield { series, column, convention: convention(&t.get_item(2)?.extract::<String>()?)? }
        }
        "curve" => {
            let series: String = t.get_item(1)?.extract()?;
            let s = ws.series(&series).map_err(crate::to_py)?;
            let curve: String = t.get_item(2)?.extract()?;
            let points: Vec<(String, f64)> = t.get_item(3)?.extract()?;
            for (c, _) in &points {
                s.column(c).map_err(crate::to_py)?;
            }
            let sp = t.get_item(6)?;
            let spread = if sp.is_none() {
                Spread::None
            } else if let Ok(x) = sp.extract::<f64>() {
                Spread::Constant(x)
            } else {
                let (series, column) = col(&sp.extract::<String>()?)?;
                Spread::Observed { series, column }
            };
            Valuation::Curve {
                series,
                curve,
                points,
                compounding: compounding(&t.get_item(4)?.extract::<String>()?)?,
                interpolation: interpolation(&t.get_item(5)?.extract::<String>()?)?,
                spread,
            }
        }
        k => return Err(PyValueError::new_err(format!("no valuation {k:?} (clean, yield, curve)"))),
    })
}

/// Debt contracts for `bonds` (name → `Bond`), entities of `kind`.
pub(crate) fn debt_contracts(
    ws: &mut Workspace,
    bonds: &Bound<'_, PyDict>,
    value: &Bound<'_, PyAny>,
    fixings: Option<&str>,
    events: Option<&str>,
    kind: &str,
) -> PyResult<Vec<Arc<dyn Contract>>> {
    let valuation = valuation(ws, value)?;
    let fixings = fixings
        .map(|f| -> PyResult<(String, String)> {
            let (s, c) = split(f)?;
            ws.series(s).and_then(|x| x.column(c)).map_err(crate::to_py)?;
            Ok((s.to_owned(), c.to_owned()))
        })
        .transpose()?;
    if let Some(e) = events {
        ws.series(e).map_err(crate::to_py)?;
    }
    let mut out: Vec<Arc<dyn Contract>> = Vec::with_capacity(bonds.len());
    for (name, bond) in bonds.iter() {
        let name: String = name.extract()?;
        let bond = bond.cast::<PyBond>().map_err(|_| PyTypeError::new_err(format!("{name}: not a Bond")))?;
        let b = bond.get();
        let entity = ws.entities.intern(kind, &name);
        let c: Arc<dyn Contract> =
            Arc::new(DebtContract { entity, terms: b.debt.clone(), valuation: valuation.clone(), fixings: fixings.clone(), events: events.map(str::to_owned) });
        out.push(match &b.currency {
            Some(ccy) => {
                let currency: EntityId = ws.entities.intern("currency", ccy);
                Arc::new(InCurrency { contract: c, currency })
            }
            None => c,
        });
    }
    Ok(out)
}

/// A swap: `receive` less `pay`, both legs valued as `value` says, on one entity of `kind`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn swap_contract(
    ws: &mut Workspace,
    name: &str,
    receive: &Bound<'_, PyAny>,
    pay: &Bound<'_, PyAny>,
    value: &Bound<'_, PyAny>,
    fixings: Option<&str>,
    kind: &str,
) -> PyResult<Vec<Arc<dyn Contract>>> {
    let valuation = valuation(ws, value)?;
    let fixings = fixings
        .map(|f| -> PyResult<(String, String)> {
            let (s, c) = split(f)?;
            ws.series(s).and_then(|x| x.column(c)).map_err(crate::to_py)?;
            Ok((s.to_owned(), c.to_owned()))
        })
        .transpose()?;
    let leg = |b: &Bound<'_, PyAny>, which: &str| -> PyResult<Arc<Debt>> {
        b.cast::<PyBond>().map(|x| x.get().debt.clone()).map_err(|_| PyTypeError::new_err(format!("{name}: its {which} leg is not a Bond")))
    };
    let (r, p) = (leg(receive, "receive")?, leg(pay, "pay")?);
    let entity = ws.entities.intern(kind, name);
    let make = |terms: Arc<Debt>| DebtContract { entity, terms, valuation: valuation.clone(), fixings: fixings.clone(), events: None };
    Ok(vec![Arc::new(fi_std::SwapContract { entity, receive: make(r), pay: make(p) })])
}
