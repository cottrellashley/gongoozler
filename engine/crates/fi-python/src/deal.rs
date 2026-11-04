//! Deals from Python (spec, change 3): a collateral book, its priority of payments, and the
//! tranches written as series. The Python package's dataclasses (`fi_engine.deals`) describe the
//! terms; this module reads them by attribute and builds a [`Deal`], a batch model.
//!
//! Steps are strings: `action[:names] [when condition]`, for example `"fee:senior"`,
//! `"interest:A"`, `"interest+reserve:A"` (a shortfall is drawn from the reserve), `"test:A_OC"`,
//! `"sequential:A,B when amortising"`, `"pro_rata:A,M,J when not sequential"`, `"schedule:PAC"`,
//! `"deficiency:A,B"`, `"reserve"`, `"to_principal"`, `"to_interest"`, `"residual:E"`.

use std::sync::Arc;

use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyDict;

use fi_kernel::{Calendar, Contract, EntityId, FillModel, MissingPolicy, ResearchFill, Ts, Workspace};
use fi_std::structured::{
    Action, Carry, Claim, Coupon, Cure, Deal, Effect, Exercise, Fee, Haircut, Losses, Manager, Metric, Notional, Right, RightKind, Shortfall, Step, Test,
    TestKind, Tranche, Trigger, When,
};

use crate::book::{PyPolicy, Schedule};
use crate::bridge::{Errors, Names};
use crate::{PyFill, convert, split};

fn bad(m: impl Into<String>) -> PyErr {
    PyValueError::new_err(m.into())
}

fn text(o: &Bound<'_, PyAny>, name: &str) -> PyResult<String> {
    o.getattr(name)?.extract()
}

fn number(o: &Bound<'_, PyAny>, name: &str) -> PyResult<f64> {
    o.getattr(name)?.extract()
}

fn maybe_number(o: &Bound<'_, PyAny>, name: &str) -> PyResult<Option<f64>> {
    let v = o.getattr(name)?;
    if v.is_none() { Ok(None) } else { v.extract().map(Some) }
}

fn maybe_time(o: &Bound<'_, PyAny>, name: &str) -> PyResult<Option<Ts>> {
    let v = o.getattr(name)?;
    if v.is_none() { Ok(None) } else { convert::ts(&v).map(Some) }
}

fn names_of(o: &Bound<'_, PyAny>) -> PyResult<Vec<String>> {
    if let Ok(s) = o.extract::<String>() {
        return Ok(vec![s]);
    }
    o.try_iter()?.map(|x| x?.extract::<String>()).collect()
}

fn list<'py>(o: Option<&Bound<'py, PyAny>>) -> PyResult<Vec<Bound<'py, PyAny>>> {
    match o.filter(|o| !o.is_none()) {
        None => Ok(Vec::new()),
        Some(o) => o.try_iter()?.collect(),
    }
}

/// The names a deal's terms refer to.
struct Index {
    tranches: Vec<String>,
    fees: Vec<String>,
    tests: Vec<String>,
    triggers: Vec<String>,
}

fn find(names: &[String], what: &str, name: &str) -> PyResult<usize> {
    names.iter().position(|n| n == name).ok_or_else(|| bad(format!("the deal has no {what} {name} (it has {names:?})")))
}

impl Index {
    fn tranche(&self, n: &str) -> PyResult<usize> {
        find(&self.tranches, "tranche", n)
    }

    fn tranches(&self, ns: &[String]) -> PyResult<Vec<usize>> {
        ns.iter().map(|n| self.tranche(n)).collect()
    }

    /// One step string.
    fn step(&self, s: &str) -> PyResult<Step> {
        let (action, when) = match s.split_once(" when ") {
            Some((a, w)) => (a.trim(), Some(w.trim())),
            None => (s.trim(), None),
        };
        let when = match when {
            None => When::Always,
            Some("reinvesting") => When::Reinvesting,
            Some("amortising" | "amortizing") => When::Amortising,
            Some(w) => match w.strip_prefix("not ") {
                Some(t) => When::NotTripped(find(&self.triggers, "trigger", t.trim())?),
                None => When::Tripped(find(&self.triggers, "trigger", w)?),
            },
        };
        let (verb, args) = match action.split_once(':') {
            Some((v, a)) => (v.trim(), a.split(',').map(|x| x.trim().to_owned()).filter(|x| !x.is_empty()).collect::<Vec<_>>()),
            None => (action, Vec::new()),
        };
        let one = |what: &str| -> PyResult<&String> {
            match args.as_slice() {
                [x] => Ok(x),
                _ => Err(bad(format!("step {s:?}: {verb} names one {what}"))),
            }
        };
        let some = || -> PyResult<Vec<usize>> { if args.is_empty() { Err(bad(format!("step {s:?} names no tranches"))) } else { self.tranches(&args) } };
        let action = match verb {
            "fee" => Action::Fee(find(&self.fees, "fee", one("fee")?)?),
            "interest" => Action::Interest { tranches: some()?, reserve: false },
            "interest+reserve" => Action::Interest { tranches: some()?, reserve: true },
            "sequential" => Action::Principal { tranches: some()?, pro_rata: false, schedule: false },
            "pro_rata" => Action::Principal { tranches: some()?, pro_rata: true, schedule: false },
            "schedule" => Action::Principal { tranches: some()?, pro_rata: false, schedule: true },
            "test" => Action::Test(find(&self.tests, "test", one("test")?)?),
            "reserve" => Action::Reserve,
            "deficiency" => Action::Deficiency(some()?),
            "to_principal" => Action::ToPrincipal,
            "to_interest" => Action::ToInterest,
            "residual" => Action::Residual(self.tranche(one("tranche")?)?),
            v => {
                return Err(bad(format!(
                    "step {s:?}: unknown action {v} (fee, interest, interest+reserve, sequential, pro_rata, schedule, test, reserve, deficiency, \
                     to_principal, to_interest, residual)"
                )));
            }
        };
        Ok(Step { action, when })
    }
}

fn coupon(o: &Bound<'_, PyAny>) -> PyResult<Coupon> {
    if o.is_none() {
        return Ok(Coupon::None);
    }
    if let Ok(r) = o.extract::<f64>() {
        return Ok(Coupon::Fixed(r));
    }
    if o.hasattr("index")? {
        let index = text(o, "index")?;
        let (s, c) = split(&index)?;
        return Ok(Coupon::Floating { index: (s.into(), c.into()), spread: number(o, "spread_bp")? * 1e-4, floor: maybe_number(o, "floor")? });
    }
    Err(PyTypeError::new_err("a coupon is None, a fixed rate, or fi.floating(index, spread_bp=...)"))
}

fn shortfall(s: &str) -> PyResult<Shortfall> {
    Ok(match s {
        "default" => Shortfall::Default,
        "defer" => Shortfall::Defer { compound: false },
        "defer_compound" => Shortfall::Defer { compound: true },
        "capitalise" | "capitalize" | "pik" => Shortfall::Capitalise,
        "extinguish" => Shortfall::Extinguish,
        s => return Err(bad(format!("shortfall is default, defer, defer_compound, capitalise or extinguish, not {s}"))),
    })
}

fn right_kind(s: &str) -> PyResult<RightKind> {
    Ok(match s {
        "call" => RightKind::Call,
        "refinance" => RightKind::Refinance,
        "reset" => RightKind::Reset,
        s => return Err(bad(format!("a right is call, refinance or reset, not {s}"))),
    })
}

/// Everything `ws.deal(...)` takes.
pub(crate) struct DealArgs<'py> {
    pub name: String,
    pub calendar: Calendar,
    pub collateral: Vec<Arc<dyn Contract>>,
    pub opening: Bound<'py, PyDict>,
    pub tranches: Bound<'py, PyAny>,
    pub payments: Option<Bound<'py, PyAny>>,
    pub interest: Vec<String>,
    pub principal: Vec<String>,
    pub fees: Option<Bound<'py, PyAny>>,
    pub tests: Option<Bound<'py, PyAny>>,
    pub haircuts: Option<Bound<'py, PyAny>>,
    pub triggers: Option<Bound<'py, PyAny>>,
    pub losses: Option<Bound<'py, PyAny>>,
    pub cash: f64,
    pub reserve: f64,
    pub reserve_target: f64,
    pub reinvest_until: Option<Bound<'py, PyAny>>,
    pub maturity: Option<Bound<'py, PyAny>>,
    pub rights: Option<Bound<'py, PyAny>>,
    pub exercises: Option<Bound<'py, PyAny>>,
    pub manager: Option<Bound<'py, PyAny>>,
    pub fill: Option<PyRef<'py, PyFill>>,
    pub missing: MissingPolicy,
    pub rounding: Option<fi_kernel::Rounding>,
    pub reads: Vec<String>,
}

/// Builds a deal from its Python description, interning its entities as `{deal}.{name}`.
pub(crate) fn build(py: Python<'_>, ws: &mut Workspace, a: DealArgs<'_>) -> PyResult<(Deal, Vec<Arc<Errors>>)> {
    let name = a.name.clone();
    let scoped = |n: &str| format!("{name}.{n}");
    let tranche_objs = list(Some(&a.tranches))?;
    let fee_objs = list(a.fees.as_ref())?;
    let test_objs = list(a.tests.as_ref())?;
    let trigger_objs = list(a.triggers.as_ref())?;
    let index = Index {
        tranches: tranche_objs.iter().map(|t| text(t, "name")).collect::<PyResult<_>>()?,
        fees: fee_objs.iter().map(|t| text(t, "name")).collect::<PyResult<_>>()?,
        tests: test_objs.iter().map(|t| text(t, "name")).collect::<PyResult<_>>()?,
        triggers: trigger_objs.iter().map(|t| text(t, "name")).collect::<PyResult<_>>()?,
    };
    let mut tranches = Vec::with_capacity(tranche_objs.len());
    for t in &tranche_objs {
        let n = text(t, "name")?;
        let entity = ws.entities.intern("tranche", &scoped(&n));
        let claim = match text(t, "claim")?.as_str() {
            "debt" => Claim::Debt,
            "equity" => Claim::Equity,
            "interest_only" => {
                let of = match text(t, "notional")?.as_str() {
                    "pool" => Notional::Pool,
                    x => Notional::Tranche(index.tranche(x)?),
                };
                Claim::InterestOnly { of, ratio: number(t, "ratio")? }
            }
            c => return Err(bad(format!("tranche {n}: a claim is debt, equity or interest_only, not {c}"))),
        };
        let mut schedule = Vec::new();
        let sched = t.getattr("schedule")?;
        if !sched.is_none() {
            let pairs = if let Ok(d) = sched.cast::<PyDict>() { d.items().into_any() } else { sched };
            for item in pairs.try_iter()? {
                let (when, balance): (Bound<'_, PyAny>, f64) = item?.extract()?;
                schedule.push((convert::ts(&when)?, balance));
            }
            schedule.sort_by_key(|(t, _)| *t);
        }
        tranches.push(Tranche {
            name: n,
            entity,
            face: number(t, "face")?,
            balance: maybe_number(t, "balance")?,
            coupon: coupon(&t.getattr("coupon")?)?,
            basis: number(t, "basis")?,
            shortfall: shortfall(&text(t, "shortfall")?)?,
            claim,
            issued: t.getattr("issued")?.extract()?,
            schedule,
        });
    }
    let fees = fee_objs
        .iter()
        .map(|f| {
            let n = text(f, "name")?;
            Ok(Fee {
                entity: ws.entities.intern("fee", &scoped(&n)),
                name: n,
                rate: number(f, "rate_bp")? * 1e-4,
                fixed: number(f, "fixed")?,
                carry: f.getattr("carry")?.extract()?,
            })
        })
        .collect::<PyResult<_>>()?;
    let mut tests = Vec::with_capacity(test_objs.len());
    for t in &test_objs {
        let n = text(t, "name")?;
        let kind = match text(t, "kind")?.as_str() {
            "oc" => TestKind::Oc,
            "ic" => TestKind::Ic,
            k => return Err(bad(format!("test {n}: a test is oc or ic, not {k}"))),
        };
        let c = t.getattr("cure")?;
        let cure = if c.is_none() {
            Cure::None
        } else if c.extract::<String>().is_ok_and(|s| s == "reinvest") {
            Cure::Reinvest
        } else {
            Cure::Paydown(index.tranches(&names_of(&c)?)?)
        };
        tests.push(Test {
            entity: ws.entities.intern("test", &scoped(&n)),
            name: n,
            kind,
            classes: index.tranches(&names_of(&t.getattr("classes")?)?)?,
            threshold: number(t, "threshold")?,
            cure,
        });
    }
    let mut triggers = Vec::with_capacity(trigger_objs.len());
    for t in &trigger_objs {
        let n = text(t, "name")?;
        let m = text(t, "metric")?;
        let metric = match m.as_str() {
            "cumulative_loss" => Metric::CumulativeLoss,
            "pool_factor" => Metric::PoolFactor,
            "excess_spread" => Metric::ExcessSpread,
            "payment_rate" => Metric::PaymentRate,
            m => match m.strip_prefix("test:") {
                Some(test) => Metric::Test(find(&index.tests, "test", test.trim())?),
                None => {
                    let (s, c) = split(m)?;
                    Metric::Series(s.into(), c.into())
                }
            },
        };
        let lookback: usize = t.getattr("lookback")?.extract()?;
        triggers.push(Trigger {
            entity: ws.entities.intern("trigger", &scoped(&n)),
            name: n,
            metric,
            above: maybe_number(t, "above")?,
            below: maybe_number(t, "below")?,
            lookback,
            curable: t.getattr("curable")?.extract()?,
            effect: if t.getattr("ends_reinvestment")?.extract()? { Effect::EndReinvestment } else { Effect::None },
        });
    }
    let mut haircuts = Vec::new();
    for h in list(a.haircuts.as_ref())? {
        let when = text(&h, "when")?;
        let (s, c) = split(&when)?;
        let mut carry = Vec::new();
        for x in h.getattr("carry")?.try_iter()? {
            let x = x?;
            if let Ok(f) = x.extract::<f64>() {
                carry.push(Carry::Fixed(f));
                continue;
            }
            let spec: String = x.extract()?;
            if spec == "market" {
                carry.push(Carry::Market);
                continue;
            }
            let (col, per) = match spec.split_once('/') {
                Some((col, per)) => (col.trim().to_owned(), per.trim().parse::<f64>().map_err(|_| bad(format!("carry {spec}: {per} is not a number")))?),
                None => (spec.clone(), 1.0),
            };
            let (cs, cc) = split(&col)?;
            carry.push(Carry::Column { series: cs.into(), column: cc.into(), per });
        }
        haircuts.push(Haircut { name: text(&h, "name")?, when: (s.into(), c.into()), carry, excess_over: maybe_number(&h, "excess_over")? });
    }
    let losses = match a.losses.as_ref().filter(|l| !l.is_none()) {
        None => Losses::Kept,
        Some(l) => {
            if l.extract::<String>().is_ok_and(|s| s == "kept") {
                Losses::Kept
            } else {
                let (kind, order, write_up): (String, Vec<String>, bool) = l.extract()?;
                let order = index.tranches(&order)?;
                match kind.as_str() {
                    "write_down" => Losses::WriteDown { order, write_up },
                    "ledger" => Losses::Ledger(order),
                    k => return Err(bad(format!("losses are \"kept\", fi.write_down(...) or fi.deficiency_ledger(...), not {k}"))),
                }
            }
        }
    };
    let rights = list(a.rights.as_ref())?
        .iter()
        .map(|r| {
            Ok(Right {
                kind: right_kind(&text(r, "kind")?)?,
                holder: index.tranche(&text(r, "holder")?)?,
                majority: number(r, "majority")?,
                from: maybe_time(r, "non_call")?.unwrap_or(Ts::MIN),
            })
        })
        .collect::<PyResult<_>>()?;
    let exercises = list(a.exercises.as_ref())?
        .iter()
        .map(|e| {
            Ok(Exercise {
                t: maybe_time(e, "date")?.ok_or_else(|| bad("an exercise needs a date"))?,
                kind: right_kind(&text(e, "kind")?)?,
                by: index.tranche(&text(e, "by")?)?,
                share: number(e, "share")?,
                redeem: index.tranches(&names_of(&e.getattr("redeem")?)?)?,
                replacements: index.tranches(&names_of(&e.getattr("replacements")?)?)?,
                expenses: number(e, "expenses")?,
                reinvest_until: maybe_time(e, "reinvest_until")?,
            })
        })
        .collect::<PyResult<_>>()?;
    let ids: Vec<EntityId> = a.collateral.iter().map(|c| c.entity()).collect();
    let names = Arc::new(Names::new(ws, &ids));
    let mut opening = Vec::with_capacity(a.opening.len());
    for (k, v) in a.opening.iter() {
        opening.push((names.id(&k.extract::<String>()?)?, v.extract::<f64>()?));
    }
    let mut manager = Vec::new();
    let mut errors = Vec::new();
    let mut parallel = true;
    let policies: Vec<Bound<'_, PyAny>> = match a.manager.as_ref().filter(|m| !m.is_none()) {
        None => Vec::new(),
        Some(m) if m.cast::<PyPolicy>().is_ok() => vec![m.clone()],
        Some(m) => m.try_iter()?.collect::<PyResult<_>>()?,
    };
    for p in &policies {
        let p = p.cast::<PyPolicy>().map_err(|_| PyTypeError::new_err("a deal's manager is a Policy or a list of Policies"))?;
        let (spec, err) = p.get().decision(py, &names)?;
        if !err.is_empty() {
            errors.extend(err);
            parallel = false;
        }
        manager.push(Manager { name: p.get().policy_name(), decide: spec, schedule: p.get().times(&a.calendar), lag: p.get().lag() });
    }
    let fill: Arc<dyn FillModel> = match &a.fill {
        Some(f) => f.build(&names)?,
        None => Arc::new(ResearchFill::default()),
    };
    let payments = Schedule::parse(a.payments.as_ref())?;
    let payments = match a.payments.as_ref().filter(|p| !p.is_none()) {
        None => a.calendar.times().to_vec(),
        Some(_) => payments.times(&a.calendar),
    };
    let entity = ws.entities.intern("deal", &name);
    let accounts = ["interest", "principal", "reserve"].map(|x| ws.entities.intern("account", &scoped(x)));
    let deal = Deal {
        name: name.clone(),
        entity,
        collateral: a.collateral,
        opening,
        cash: a.cash,
        reserve: a.reserve,
        reserve_target: a.reserve_target,
        interest: a.interest.iter().map(|s| index.step(s)).collect::<PyResult<_>>()?,
        principal: a.principal.iter().map(|s| index.step(s)).collect::<PyResult<_>>()?,
        tranches,
        fees,
        haircuts,
        tests,
        triggers,
        losses,
        payments,
        reinvest_until: a.reinvest_until.as_ref().filter(|x| !x.is_none()).map(convert::ts).transpose()?,
        maturity: a.maturity.as_ref().filter(|x| !x.is_none()).map(convert::ts).transpose()?,
        rights,
        exercises,
        manager,
        fill,
        missing: a.missing,
        rounding: a.rounding,
        accounts,
        reads: a.reads,
        parallel,
    };
    deal.validate().map_err(crate::to_py)?;
    Ok((deal, errors))
}
