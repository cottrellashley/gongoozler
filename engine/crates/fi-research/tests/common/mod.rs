//! Deals for the tests: loans with supplied flows, and deals with no terms but their tranches.

#![allow(dead_code, unused_macros, unused_imports)]

use std::sync::Arc;

use fi_kernel::{Calendar, Column, Contract, EntityId, Graph, MissingPolicy, Model, ResearchFill, Series, Ts, Unit, Workspace};
use fi_std::SuppliedBond;
use fi_std::structured::{Action, Coupon, Cure, Deal, Losses, Step, Test, TestKind, Tranche};

/// Payment dates 90 days apart: a quarter is exactly 0.25 of a year, Act/360.
pub fn day(k: i64) -> Ts {
    Ts::from_days(19_000 + 90 * k)
}

pub fn calendar(n: i64) -> Calendar {
    Calendar::new((0..n).map(day).collect())
}

/// A loan: a constant clean price, and flows per unit of original face at calendar indices:
/// (index, interest, principal repaid, written down, written up).
pub struct Loan {
    pub name: &'static str,
    pub clean: f64,
    pub flows: Vec<(usize, f64, f64, f64, f64)>,
}

pub fn loan(name: &'static str, clean: f64, flows: Vec<(usize, f64, f64, f64, f64)>) -> Loan {
    Loan { name, clean, flows }
}

/// Registers the loans' marks and flows; returns their contracts and entities.
pub fn pool(ws: &mut Workspace, cal: &Calendar, loans: &[Loan]) -> (Vec<Arc<dyn Contract>>, Vec<EntityId>) {
    let times = cal.times().to_vec();
    let ids: Vec<EntityId> = loans.iter().map(|l| ws.entities.intern("loan", l.name)).collect();
    let n = loans.len();
    let mut clean = vec![0.0; times.len() * n];
    for k in 0..times.len() {
        for (j, l) in loans.iter().enumerate() {
            clean[k * n + j] = l.clean;
        }
    }
    let marks = vec![Column::new("clean", Unit::Price, clean), Column::new("accrued", Unit::Price, vec![0.0; times.len() * n])];
    ws.add_series(Series::grid("loans", ids.clone(), times.clone(), times.clone(), marks).unwrap()).unwrap();
    let (mut e, mut t) = (Vec::new(), Vec::new());
    let mut v = vec![Vec::new(); 4];
    for (j, l) in loans.iter().enumerate() {
        for &(k, i, p, d, u) in &l.flows {
            e.push(ids[j]);
            t.push(times[k]);
            for (c, x) in [i, p, d, u].into_iter().enumerate() {
                v[c].push(x);
            }
        }
    }
    let names = ["interest", "principal_face", "writedown_face", "writeup_face"];
    let cols = names.iter().zip(v).map(|(c, v)| Column::new(*c, Unit::Number, v)).collect();
    ws.add_series(Series::rows("loan_flows", e, t.clone(), t, cols).unwrap()).unwrap();
    let contracts = ids
        .iter()
        .map(|&entity| {
            Arc::new(SuppliedBond {
                entity,
                marks: "loans".into(),
                clean: "clean".into(),
                accrued: "accrued".into(),
                flows: Some("loan_flows".into()),
                factor: None,
                reconcile: true,
            }) as Arc<dyn Contract>
        })
        .collect();
    (contracts, ids)
}

pub fn tranche(ws: &mut Workspace, name: &str) -> EntityId {
    ws.entities.intern("tranche", name)
}

pub fn debt(ws: &mut Workspace, name: &str, face: f64, rate: f64) -> Tranche {
    let e = tranche(ws, name);
    Tranche::debt(name, e, face, Coupon::Fixed(rate))
}

pub fn equity(ws: &mut Workspace, name: &str, face: f64) -> Tranche {
    let e = tranche(ws, name);
    Tranche::equity(name, e, face)
}

/// A deal with no fees, tests, triggers, steps or rights: each case adds what it needs.
pub fn deal(
    ws: &mut Workspace,
    name: &str,
    collateral: Vec<Arc<dyn Contract>>,
    opening: Vec<(EntityId, f64)>,
    tranches: Vec<Tranche>,
    payments: Vec<Ts>,
) -> Deal {
    let entity = ws.entities.intern("deal", name);
    let accounts = ["interest", "principal", "reserve"].map(|a| ws.entities.intern("account", &format!("{name}.{a}")));
    Deal {
        name: name.into(),
        entity,
        collateral,
        opening,
        cash: 0.0,
        reserve: 0.0,
        reserve_target: 0.0,
        tranches,
        fees: vec![],
        haircuts: vec![],
        tests: vec![],
        triggers: vec![],
        interest: vec![],
        principal: vec![],
        losses: Losses::Kept,
        payments,
        reinvest_until: None,
        maturity: None,
        rights: vec![],
        exercises: vec![],
        manager: vec![],
        fill: Arc::new(ResearchFill::default()),
        missing: MissingPolicy::Fail,
        rounding: None,
        accounts,
        reads: vec![],
        parallel: true,
    }
}

pub fn step(a: Action) -> Step {
    Step::always(a)
}

pub fn interest(js: &[usize]) -> Step {
    step(Action::Interest { tranches: js.to_vec(), reserve: false })
}

pub fn sequential(js: &[usize]) -> Step {
    step(Action::Principal { tranches: js.to_vec(), pro_rata: false, schedule: false })
}

pub fn oc(ws: &mut Workspace, name: &str, classes: &[usize], threshold: f64, cure: Cure) -> Test {
    let entity = ws.entities.intern("test", name);
    Test { name: name.into(), entity, kind: TestKind::Oc, classes: classes.to_vec(), threshold, cure }
}

pub fn evaluate(ws: &mut Workspace, cal: &Calendar, deals: Vec<Deal>) -> fi_kernel::Result<()> {
    let mut g = Graph::new();
    for d in deals {
        g.add(Model::batch(d));
    }
    g.evaluate(ws, cal).map(|_| ())
}

/// A value at exactly `t` (a grid cell, or a row's value), NaN when there is none.
pub fn at(ws: &Workspace, series: &str, column: &str, e: EntityId, t: Ts) -> f64 {
    let s = ws.series(series).unwrap();
    s.exact(s.column(column).unwrap(), e, t, Ts::MAX).map_or(f64::NAN, |h| h.value)
}

/// A rows value at `t`, zero when there is no row.
pub fn flow(ws: &Workspace, series: &str, column: &str, e: EntityId, t: Ts) -> f64 {
    let v = at(ws, series, column, e, t);
    if v.is_nan() { 0.0 } else { v }
}

/// Every row's value of a rows series' column, summed over an entity's rows.
pub fn total(ws: &Workspace, series: &str, column: &str, e: EntityId) -> f64 {
    let s = ws.series(series).unwrap();
    let c = s.column(column).unwrap();
    s.rows_between(e, Ts::MIN, Ts::MAX, Ts::MAX).iter().map(|&r| s.value(c, r)).sum()
}

pub fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * b.abs().max(1.0)
}

#[macro_export]
macro_rules! assert_close {
    ($a:expr, $b:expr) => {{
        let (a, b) = ($a, $b);
        assert!(close(a, b), "{} = {a}, expected {b}", stringify!($a));
    }};
}
