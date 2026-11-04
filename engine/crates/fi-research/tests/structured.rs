//! A book is a contract (spec, change 3): deals, their priorities of payments, and the brief's
//! structured fixtures. Each case is a small synthetic deal with a hand-calculated result: F01,
//! F02, F03, F04, F05, F06, F14 and F18, plus R9 and R12.
//!
//! One producer per state (change 4): observations stand in for derived states only when nothing
//! upstream is intervened (F03, R10, P6), interventions propagate, and a run's mode is derived.
//! Holdings replays reconcile to official figures with the differences visible (P2, E4).

use std::sync::Arc;

use fi_kernel::{
    Calendar, Column, Contract, DecisionModel, EntityId, ExecRef, Graph, Input, Intervention, KnowledgePolicy, Ledger, LedgerConfig, MissingPolicy, Mode,
    Model, ModelRole, ModelSpec, Producer, Provenance, ResearchFill, Rounding, RoundingMode, Series, Ts, Unit, UnitRecords, Workspace,
};
use fi_research::{Book, Executor, PolicySpec, RunMode, reconcile, run_book, run_mode};
use fi_std::structured::{
    Action, Carry, Claim, Coupon, Cure, Deal, Effect, Exercise, Fee, Haircut, Losses, Manager, Metric, Notional, Right, RightKind, Shortfall, Step, Test,
    TestKind, Tranche, Trigger, When,
};
use fi_std::{Branch, Reinvest, Replay, SuppliedBond, TopN, Trades, ZScore};

mod common;
use common::*;

// ------------------------------------------------------------------------------------------- F01

/// The F01 control: numerator `par`, debt 100, threshold 1.03, cured from interest.
fn f01(par: f64) -> (Workspace, Calendar, EntityId, EntityId, EntityId) {
    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let cal = calendar(2);
    let (c, ids) = pool(&mut ws, &cal, &[loan("L", 100.0, vec![(1, 5.0 / par, 0.0, 0.0, 0.0)])]);
    let a = debt(&mut ws, "A", 100.0, 0.0);
    let e = equity(&mut ws, "E", 10.0);
    let (ae, ee) = (a.entity, e.entity);
    let mut d = deal(&mut ws, "clo", c, vec![(ids[0], par)], vec![a, e], vec![day(1)]);
    let t = oc(&mut ws, "A_OC", &[0], 1.03, Cure::Paydown(vec![0]));
    let te = t.entity;
    d.tests = vec![t];
    d.interest = vec![interest(&[0]), step(Action::Test(0)), step(Action::Residual(1))];
    evaluate(&mut ws, &cal, vec![d]).unwrap();
    (ws, cal, ae, ee, te)
}

#[test]
fn f01_the_cure_is_debt_less_numerator_over_threshold() {
    let (ws, _, a, e, test) = f01(102.0);
    let t = day(1);
    let cure = 100.0 - 102.0 / 1.03;
    assert_close!(cure, 0.970_873_786_407_767);
    assert_close!(at(&ws, "clo.tests", "value", test, t), 1.02);
    assert_eq!(at(&ws, "clo.tests", "passed", test, t), 0.0);
    assert_close!(at(&ws, "clo.tests", "cure", test, t), cure);
    // Recalculated after the cure, the numerator is unchanged: interest was never part of it.
    assert_close!(at(&ws, "clo.tests", "after", test, t), 1.03);
    assert_close!(flow(&ws, "clo.flows", "principal_face", a, t) * 100.0, cure);
    assert_close!(flow(&ws, "clo.flows", "interest", e, t) * 10.0, 5.0 - cure);
    assert_close!(at(&ws, "clo.marks", "balance", a, t), 100.0 - cure);
}

#[test]
fn f01_exactly_at_the_threshold_passes_and_just_below_cures() {
    let t = day(1);
    let (ws, _, a, _, test) = f01(103.0);
    assert_eq!(at(&ws, "clo.tests", "passed", test, t), 1.0);
    assert_eq!(at(&ws, "clo.tests", "cure", test, t), 0.0);
    assert_eq!(flow(&ws, "clo.flows", "principal_face", a, t), 0.0);
    let (ws, _, _, _, test) = f01(103.0001);
    assert_eq!(at(&ws, "clo.tests", "passed", test, t), 1.0);
    let (ws, _, _, _, test) = f01(102.9999);
    assert_eq!(at(&ws, "clo.tests", "passed", test, t), 0.0);
    assert_close!(at(&ws, "clo.tests", "cure", test, t), 100.0 - 102.9999 / 1.03);
}

#[test]
fn f01_curing_from_principal_also_lowers_the_numerator() {
    // Par 62 and principal cash 40: the same numerator, 102. Paying principal cash out lowers both
    // sides, so the cure is (1.03 × 100 − 102) / 0.03, not 0.97.
    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let cal = calendar(2);
    let (c, ids) = pool(&mut ws, &cal, &[loan("L", 100.0, vec![])]);
    let a = debt(&mut ws, "A", 100.0, 0.0);
    let ae = a.entity;
    let mut d = deal(&mut ws, "clo", c, vec![(ids[0], 62.0)], vec![a], vec![day(1)]);
    d.cash = 40.0;
    let t = oc(&mut ws, "A_OC", &[0], 1.03, Cure::Paydown(vec![0]));
    let te = t.entity;
    d.tests = vec![t];
    d.principal = vec![step(Action::Test(0))];
    evaluate(&mut ws, &cal, vec![d]).unwrap();
    let cure = (1.03 * 100.0 - 102.0) / 0.03;
    assert_close!(at(&ws, "clo.tests", "cure", te, day(1)), cure);
    assert_close!(at(&ws, "clo.tests", "after", te, day(1)), 1.03);
    assert_close!(flow(&ws, "clo.flows", "principal_face", ae, day(1)) * 100.0, cure);
}

#[test]
fn f01_adjusted_collateral_follows_the_declared_haircuts_and_what_was_known() {
    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let cal = calendar(3);
    let loans = [
        loan("P", 98.0, vec![]),
        loan("D", 30.0, vec![]),
        loan("C1", 70.0, vec![]),
        loan("C2", 80.0, vec![]),
        loan("X", 90.0, vec![]),
        loan("Q", 60.0, vec![]),
    ];
    let (c, ids) = pool(&mut ws, &cal, &loans);
    let [p, dd, c1, c2, x, q] = [0, 1, 2, 3, 4, 5].map(|i| ids[i]);
    // Flags with their publication times: C2's downgrade was reported before the test date, Q's
    // only after it.
    let (t0, t1) = (day(0), day(1));
    let rows = [
        (dd, t0, t0, [1.0, 0.0, 0.0]),
        (x, t0, t0, [0.0, 1.0, 0.0]),
        (c1, t0, t0, [0.0, 0.0, 1.0]),
        (c2, t1.plus(-10 * Ts::DAY), t1.plus(-5 * Ts::DAY), [0.0, 0.0, 1.0]),
        (q, t1.plus(-10 * Ts::DAY), t1.plus(5 * Ts::DAY), [0.0, 0.0, 1.0]),
    ];
    let mut cols = vec![Vec::new(); 3];
    for r in &rows {
        for (c, v) in r.3.iter().enumerate() {
            cols[c].push(*v);
        }
    }
    let flags = ["defaulted", "discount", "ccc"].iter().zip(cols).map(|(n, v)| Column::new(*n, Unit::Flag, v)).collect();
    ws.add_series(
        Series::rows("flags", rows.iter().map(|r| r.0).collect(), rows.iter().map(|r| r.1).collect(), rows.iter().map(|r| r.2).collect(), flags).unwrap(),
    )
    .unwrap();
    let terms = vec![Column::new("recovery", Unit::Rate, vec![0.4, f64::NAN]), Column::new("purchase", Unit::Price, vec![f64::NAN, 75.0])];
    ws.add_series(Series::rows("terms", vec![dd, x], vec![t0, t0], vec![t0, t0], terms).unwrap()).unwrap();
    let a = debt(&mut ws, "A", 80.0, 0.0);
    let opening = vec![(p, 60.0), (dd, 10.0), (c1, 8.0), (c2, 7.0), (x, 15.0), (q, 5.0)];
    let mut d = deal(&mut ws, "clo", c, opening, vec![a], vec![day(1), day(2)]);
    d.haircuts = vec![
        Haircut {
            name: "defaulted".into(),
            when: ("flags".into(), "defaulted".into()),
            carry: vec![Carry::Market, Carry::Column { series: "terms".into(), column: "recovery".into(), per: 1.0 }],
            excess_over: None,
        },
        Haircut {
            name: "discount".into(),
            when: ("flags".into(), "discount".into()),
            carry: vec![Carry::Column { series: "terms".into(), column: "purchase".into(), per: 100.0 }],
            excess_over: None,
        },
        Haircut { name: "ccc".into(), when: ("flags".into(), "ccc".into()), carry: vec![Carry::Market], excess_over: Some(0.075) },
    ];
    let t = oc(&mut ws, "A_OC", &[0], 1.0, Cure::None);
    let te = t.entity;
    d.tests = vec![t];
    d.interest = vec![step(Action::Test(0))];
    evaluate(&mut ws, &cal, vec![d]).unwrap();
    // At the first test: the defaulted loan at the lesser of 30% and its 40% recovery (3); the
    // discount obligation at its purchase price (11.25); CCC above 7.5% of the remaining 80 of par
    // (9 of 15) at market, lowest first: C1's 8 at 70% and 1 of C2 at 80%.
    let first = 105.0 - 7.0 - 3.75 - (8.0 * 0.3 + 1.0 * 0.2);
    assert_close!(at(&ws, "clo.tests", "value", te, day(1)) * 80.0, first);
    // Once Q's downgrade is known, it is the lowest-priced CCC, and 14 of 20 is excess.
    let second = 105.0 - 7.0 - 3.75 - (5.0 * 0.4 + 8.0 * 0.3 + 1.0 * 0.2);
    assert_close!(at(&ws, "clo.tests", "value", te, day(2)) * 80.0, second);
}

// ------------------------------------------------------------------------------------------- F02

struct F02 {
    ws: Workspace,
    a: EntityId,
    b: EntityId,
    e: EntityId,
    senior: EntityId,
    ic: EntityId,
}

/// Interest of 2.5, 1.6, then 3.5: enough for the seniors in the stress period, not for B.
fn f02() -> F02 {
    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let cal = calendar(4);
    let (c, ids) = pool(&mut ws, &cal, &[loan("L", 100.0, vec![(1, 0.025, 0.0, 0.0, 0.0), (2, 0.016, 0.0, 0.0, 0.0), (3, 0.035, 0.0, 0.0, 0.0)])]);
    let a = debt(&mut ws, "A", 70.0, 0.08);
    let mut b = debt(&mut ws, "B", 20.0, 0.12);
    b.shortfall = Shortfall::Defer { compound: true };
    let e = equity(&mut ws, "E", 10.0);
    let (ae, be, ee) = (a.entity, b.entity, e.entity);
    let mut d = deal(&mut ws, "clo", c, vec![(ids[0], 100.0)], vec![a, b, e], vec![day(1), day(2), day(3)]);
    let senior = ws.entities.intern("fee", "senior");
    d.fees = vec![Fee { name: "senior".into(), entity: senior, rate: 0.0, fixed: 0.1, carry: true }];
    let ic = ws.entities.intern("test", "A_IC");
    d.tests = vec![Test { name: "A_IC".into(), entity: ic, kind: TestKind::Ic, classes: vec![0], threshold: 1.2, cure: Cure::Paydown(vec![0]) }];
    d.interest = vec![step(Action::Fee(0)), interest(&[0]), step(Action::Test(0)), interest(&[1]), step(Action::Residual(2))];
    d.principal = vec![sequential(&[0, 1])];
    evaluate(&mut ws, &cal, vec![d]).unwrap();
    F02 { ws, a: ae, b: be, e: ee, senior, ic }
}

#[test]
fn f02_an_ic_shortfall_defers_junior_interest_which_is_paid_later_in_priority() {
    let F02 { ws, a, b, e, ic, .. } = f02();
    let (t1, t2, t3) = (day(1), day(2), day(3));
    // Period 1: everything is paid. 2.5 = 0.1 fee + 1.4 A + 0.6 B + 0.4 equity.
    assert_close!(flow(&ws, "clo.flows", "interest", a, t1) * 70.0, 1.4);
    assert_close!(flow(&ws, "clo.flows", "interest", b, t1) * 20.0, 0.6);
    assert_close!(flow(&ws, "clo.flows", "interest", e, t1) * 10.0, 0.4);
    // Period 2: the test fails (1.5 / 1.4), and the 0.1 left pays A's principal; B is deferred;
    // the equity receives nothing allocated to senior protection.
    assert_close!(at(&ws, "clo.tests", "value", ic, t2), 1.5 / 1.4);
    assert_close!(at(&ws, "clo.tests", "cure", ic, t2), 0.1);
    assert_close!(flow(&ws, "clo.flows", "principal_face", a, t2) * 70.0, 0.1);
    assert_eq!(flow(&ws, "clo.flows", "interest", b, t2), 0.0);
    assert_eq!(flow(&ws, "clo.flows", "interest", e, t2), 0.0);
    assert_close!(at(&ws, "clo.marks", "deferred", b, t2), 0.6);
    // Period 3: the deferred 0.6 earned B's coupon for the quarter (0.018) and is paid with the
    // current 0.6, after A's interest on its reduced balance (69.9 × 2%).
    assert_close!(flow(&ws, "clo.flows", "interest", a, t3) * 70.0, 1.398);
    assert_close!(flow(&ws, "clo.flows", "interest", b, t3) * 20.0, 0.6 + 0.618);
    assert_eq!(at(&ws, "clo.marks", "deferred", b, t3), 0.0);
    assert_close!(flow(&ws, "clo.flows", "interest", e, t3) * 10.0, 3.5 - 0.1 - 1.398 - 1.218);
}

#[test]
fn f02_distributions_reconcile_to_the_cash_collected() {
    let F02 { ws, a, b, e, senior, .. } = f02();
    for (k, collected) in [(1, 2.5), (2, 1.6), (3, 3.5)] {
        let t = day(k);
        let paid: f64 =
            [a, b, e, senior].iter().map(|r| ["interest", "principal", "fee"].iter().map(|c| flow(&ws, "clo.distributions", c, *r, t)).sum::<f64>()).sum();
        let deal = ws.entities.get("deal", "clo").unwrap();
        assert_close!(at(&ws, "clo.collateral", "interest", deal, t), collected);
        assert_close!(paid, collected);
    }
}

// ------------------------------------------------------------------------------------------- F03

/// A CLO whose manager trades L2 for L3 at t2 (recorded), or, in the counterfactual, for L4,
/// which defaults at t3.
fn f03(ws: &mut Workspace, cal: &Calendar, name: &str, manager: Arc<dyn DecisionModel>, collateral: &[Arc<dyn Contract>], ids: &[EntityId]) -> Deal {
    let a = Tranche::debt("A", ws.entities.intern("tranche", "A"), 80.0, Coupon::Fixed(0.04));
    let e = Tranche::equity("E", ws.entities.intern("tranche", "E"), 20.0);
    let mut d = deal(ws, name, collateral.to_vec(), vec![(ids[0], 50.0), (ids[1], 50.0)], vec![a, e], (1..5).map(day).collect());
    d.tests = vec![oc(ws, &format!("{name}.A_OC"), &[0], 1.1, Cure::Paydown(vec![0]))];
    d.interest = vec![interest(&[0]), step(Action::Test(0)), step(Action::Residual(1))];
    d.manager = vec![Manager { name: "manager".into(), decide: manager, schedule: cal.times().to_vec(), lag: 0 }];
    d.reads = vec!["trades".into(), "alt".into()];
    d
}

#[test]
fn f03_a_branch_without_change_reproduces_the_baseline_and_a_changed_trade_recomputes_the_deal() {
    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let cal = calendar(5);
    let paying = |k: usize| (k, 0.02, 0.0, 0.0, 0.0);
    let loans = [
        loan("L1", 100.0, (1..5).map(paying).collect()),
        loan("L2", 100.0, (1..5).map(paying).collect()),
        loan("L3", 100.0, (1..5).map(paying).collect()),
        loan("L4", 100.0, vec![paying(1), paying(2), (3, 0.0, 0.0, 0.5, 0.0), (4, 0.0, 0.0, 0.0, 0.0)]),
    ];
    let (c, ids) = pool(&mut ws, &cal, &loans);
    let t2 = day(2);
    for (name, buy) in [("trades", ids[2]), ("alt", ids[3])] {
        let cols = vec![Column::new("qty", Unit::Number, vec![-50.0, 50.0])];
        ws.add_series(Series::rows(name, vec![ids[1], buy], vec![t2, t2], vec![t2, t2], cols).unwrap()).unwrap();
    }
    let trades = |s: &str| Arc::new(Trades { series: s.into(), column: "qty".into(), universe: ids.clone() }) as Arc<dyn DecisionModel>;
    let base = f03(&mut ws, &cal, "base", trades("trades"), &c, &ids);
    let same = Arc::new(Branch { at: t2, before: trades("trades"), after: trades("trades") });
    let branch = f03(&mut ws, &cal, "branch", same, &c, &ids);
    let changed = Arc::new(Branch { at: t2, before: trades("trades"), after: trades("alt") });
    let cf = f03(&mut ws, &cal, "cf", changed, &c, &ids);
    evaluate(&mut ws, &cal, vec![base, branch, cf]).unwrap();
    let (a, e) = (ws.entities.get("tranche", "A").unwrap(), ws.entities.get("tranche", "E").unwrap());
    for k in 1..5 {
        let t = day(k);
        for (s, c) in [("flows", "interest"), ("flows", "principal_face"), ("marks", "balance"), ("marks", "clean")] {
            for x in [a, e] {
                let (b, r) = (at(&ws, &format!("base.{s}"), c, x, t), at(&ws, &format!("branch.{s}"), c, x, t));
                assert!(b == r || (b.is_nan() && r.is_nan()), "{s}.{c} differs at {k}: {b} vs {r}");
            }
        }
    }
    // The counterfactual holds L4: its default at t3 is an exogenous outcome the manager could
    // not see at t2, and the deal's own tests and cash flows follow from the changed pool.
    let deal = |n: &str| ws.entities.get("deal", n).unwrap();
    assert_close!(at(&ws, "base.collateral", "par", deal("base"), day(3)), 100.0);
    assert_close!(at(&ws, "cf.collateral", "par", deal("cf"), day(3)), 75.0);
    assert!(flow(&ws, "base.flows", "interest", e, day(3)) > 0.0);
    assert_eq!(flow(&ws, "cf.flows", "interest", e, day(3)), 0.0);
    assert!(flow(&ws, "cf.flows", "principal_face", a, day(3)) > 0.0);
    assert_eq!(flow(&ws, "base.flows", "interest", e, day(2)), flow(&ws, "cf.flows", "interest", e, day(2)));
}

// ------------------------------------------------------------------------------------------- F04

struct F04 {
    ws: Workspace,
    a: EntityId,
    ar: EntityId,
    e: EntityId,
}

fn f04(exercise: impl Fn(usize, usize, usize) -> Exercise) -> fi_kernel::Result<F04> {
    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let cal = calendar(7);
    let times = cal.times().to_vec();
    let n = times.len();
    ws.add_series(Series::rows("sofr", vec![EntityId::NONE; n], times.clone(), times, vec![Column::new("rate", Unit::Rate, vec![0.04; n])]).unwrap()).unwrap();
    let (c, ids) = pool(&mut ws, &cal, &[loan("L", 100.0, (1..7).map(|k| (k, 0.02, 0.0, 0.0, 0.0)).collect())]);
    let floating = |spread: f64| Coupon::Floating { index: ("sofr".into(), "rate".into()), spread, floor: None };
    let a = Tranche::debt("A", tranche(&mut ws, "A"), 80.0, floating(0.015));
    let mut b = debt(&mut ws, "B", 10.0, 0.06);
    b.shortfall = Shortfall::Defer { compound: false };
    let e = equity(&mut ws, "E", 10.0);
    let mut ar = Tranche::debt("A-R", tranche(&mut ws, "A-R"), 80.0, floating(0.011));
    ar.issued = false;
    let (ae, ee, are) = (a.entity, e.entity, ar.entity);
    let mut d = deal(&mut ws, "clo", c, vec![(ids[0], 100.0)], vec![a, b, e, ar], (1..7).map(day).collect());
    d.reinvest_until = Some(day(5));
    d.rights = [RightKind::Refinance, RightKind::Reset, RightKind::Call].map(|kind| Right { kind, holder: 2, majority: 0.5, from: day(2) }).to_vec();
    d.exercises = vec![exercise(0, 3, 2)];
    d.interest = vec![interest(&[0]), interest(&[1]), step(Action::Residual(2))];
    d.principal = vec![Step { action: Action::Principal { tranches: vec![0, 1], pro_rata: false, schedule: false }, when: When::Amortising }];
    evaluate(&mut ws, &cal, vec![d])?;
    Ok(F04 { ws, a: ae, ar: are, e: ee })
}

fn refinance(kind: RightKind, at: i64, by: usize, share: f64) -> impl Fn(usize, usize, usize) -> Exercise {
    move |old, new, _| Exercise {
        t: day(at),
        kind,
        by,
        share,
        redeem: vec![old],
        replacements: vec![new],
        expenses: 0.05,
        reinvest_until: (kind == RightKind::Reset).then(|| day(6)),
    }
}

#[test]
fn f04_a_refinancing_redeems_the_old_claim_once_and_issues_its_replacement() {
    let F04 { ws, a, ar, e } = f04(refinance(RightKind::Refinance, 3, 2, 0.6)).unwrap();
    let t3 = day(3);
    // At the refinancing, A is paid its interest (80 × 5.5% / 4) and redeemed at par, once.
    assert_close!(flow(&ws, "clo.flows", "interest", a, t3) * 80.0, 1.1);
    assert_close!(flow(&ws, "clo.flows", "principal_face", a, t3), 1.0);
    assert_close!(total(&ws, "clo.flows", "principal_face", a), 1.0);
    assert_eq!(flow(&ws, "clo.flows", "interest", a, day(4)), 0.0);
    // The expenses come out of interest before the equity: 2.0 − 0.05 − 1.1 − 0.15.
    assert_close!(flow(&ws, "clo.flows", "interest", e, t3) * 10.0, 0.7);
    // A-R is a new claim, linked to the one it replaces, accruing at its own spread.
    assert_close!(at(&ws, "clo.claims", "balance", ar, t3), 80.0);
    assert_eq!(at(&ws, "clo.claims", "replaces", ar, t3), a.as_value());
    assert_close!(flow(&ws, "clo.flows", "interest", ar, day(4)) * 80.0, 80.0 * 0.051 / 4.0);
    assert!(at(&ws, "clo.marks", "balance", ar, day(2)).is_nan());
    // A refinancing does not extend reinvestment.
    let deal = ws.entities.get("deal", "clo").unwrap();
    assert_eq!(at(&ws, "clo.collateral", "reinvesting", deal, day(4)), 1.0);
    assert_eq!(at(&ws, "clo.collateral", "reinvesting", deal, day(5)), 0.0);
}

#[test]
fn f04_a_reset_extends_reinvestment_as_its_terms_say() {
    let F04 { ws, .. } = f04(refinance(RightKind::Reset, 3, 2, 0.6)).unwrap();
    let deal = ws.entities.get("deal", "clo").unwrap();
    assert_eq!(at(&ws, "clo.collateral", "reinvesting", deal, day(5)), 1.0);
    assert_eq!(at(&ws, "clo.collateral", "reinvesting", deal, day(6)), 0.0);
}

#[test]
fn f04_forbidden_exercises_are_refused() {
    let early = f04(refinance(RightKind::Refinance, 1, 2, 0.6)).err().unwrap().to_string();
    assert!(early.contains("non-call"), "{early}");
    let passive = f04(refinance(RightKind::Refinance, 3, 1, 1.0)).err().unwrap().to_string();
    assert!(passive.contains("holds no right"), "{passive}");
    let minority = f04(refinance(RightKind::Refinance, 3, 2, 0.4)).err().unwrap().to_string();
    assert!(minority.contains("to consent"), "{minority}");
}

#[test]
fn a_call_sells_the_collateral_and_pays_by_seniority() {
    let call = |_: usize, _: usize, by: usize| Exercise {
        t: day(4),
        kind: RightKind::Call,
        by,
        share: 1.0,
        redeem: vec![],
        replacements: vec![],
        expenses: 0.0,
        reinvest_until: None,
    };
    let F04 { ws, a, e, .. } = f04(call).unwrap();
    assert_close!(flow(&ws, "clo.flows", "principal_face", a, day(4)), 1.0);
    // The equity receives the rest: 100 of collateral and 2.0 of interest, less A, B and their
    // interest.
    let interest = 80.0 * 0.055 / 4.0 + 10.0 * 0.06 / 4.0;
    assert_close!(flow(&ws, "clo.flows", "interest", e, day(4)) * 10.0, 102.0 - 80.0 - 10.0 - interest);
    assert_eq!(flow(&ws, "clo.flows", "writedown_face", e, day(4)), 1.0);
    assert_eq!(flow(&ws, "clo.flows", "interest", e, day(5)), 0.0);
}

// ------------------------------------------------------------------------------- F05 and R12

fn f05(curable: bool) -> (Workspace, EntityId, EntityId) {
    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let cal = calendar(7);
    let times = cal.times().to_vec();
    // Revolving receivables: R repays 20 of par each period; principal collections buy N.
    let (c, ids) = pool(&mut ws, &cal, &[loan("R", 100.0, (1..6).map(|k| (k, 0.015, 0.2, 0.0, 0.0)).collect()), loan("N", 100.0, vec![])]);
    // The trigger's metric, as observed: its three-period average is exactly 0 at t3, -1 at t4.
    let perf = vec![2.0, 1.0, -3.0, -1.0, 5.0, 5.0];
    ws.add_series(
        Series::rows("perf", vec![EntityId::NONE; 6], times[1..].to_vec(), times[1..].to_vec(), vec![Column::new("excess", Unit::Number, perf)]).unwrap(),
    )
    .unwrap();
    let a = debt(&mut ws, "A", 80.0, 0.0);
    let e = equity(&mut ws, "E", 20.0);
    let ae = a.entity;
    let mut d = deal(&mut ws, "abs", c, vec![(ids[0], 100.0)], vec![a, e], (1..7).map(day).collect());
    d.reinvest_until = Some(day(10));
    let te = ws.entities.intern("trigger", "early_amortization");
    d.triggers = vec![Trigger {
        name: "early_amortization".into(),
        entity: te,
        metric: Metric::Series("perf".into(), "excess".into()),
        above: None,
        below: Some(0.0),
        lookback: 3,
        curable,
        effect: Effect::EndReinvestment,
    }];
    d.interest = vec![step(Action::Residual(1))];
    d.principal = vec![Step { action: Action::Principal { tranches: vec![0], pro_rata: false, schedule: false }, when: When::Amortising }];
    d.manager = vec![Manager { name: "replenish".into(), decide: Arc::new(Reinvest { into: vec![ids[1]], keep: 0.0 }), schedule: times, lag: 0 }];
    evaluate(&mut ws, &cal, vec![d]).unwrap();
    (ws, ae, te)
}

#[test]
fn f05_r12_early_amortization_trips_at_the_observation_point_and_stops_purchases() {
    let (ws, a, trig) = f05(false);
    let deal = ws.entities.get("deal", "abs").unwrap();
    // Exactly at the boundary (average 0) the trigger does not trip.
    assert_eq!(at(&ws, "abs.tests", "value", trig, day(3)), 0.0);
    assert_eq!(at(&ws, "abs.tests", "passed", trig, day(3)), 1.0);
    assert_eq!(at(&ws, "abs.tests", "passed", trig, day(4)), 0.0);
    // While revolving, collections buy receivables and the notes receive no principal.
    for k in 1..5 {
        assert_eq!(flow(&ws, "abs.flows", "principal_face", a, day(k)), 0.0);
    }
    assert_eq!(at(&ws, "abs.collateral", "reinvesting", deal, day(3)), 1.0);
    assert_eq!(at(&ws, "abs.collateral", "reinvesting", deal, day(4)), 0.0);
    // From the next decision, purchases stop and principal goes to the notes. A permanent
    // trigger stays tripped though the metric recovers.
    assert_close!(flow(&ws, "abs.flows", "principal_face", a, day(5)) * 80.0, 20.0);
    assert_eq!(at(&ws, "abs.tests", "passed", trig, day(5)), 0.0);
    assert_eq!(at(&ws, "abs.collateral", "reinvesting", deal, day(6)), 0.0);
}

#[test]
fn f05_a_curable_trigger_clears_when_the_metric_recovers() {
    let (ws, a, trig) = f05(true);
    let deal = ws.entities.get("deal", "abs").unwrap();
    assert_eq!(at(&ws, "abs.tests", "passed", trig, day(4)), 0.0);
    assert_eq!(at(&ws, "abs.tests", "passed", trig, day(5)), 1.0);
    assert_eq!(at(&ws, "abs.collateral", "reinvesting", deal, day(5)), 1.0);
    assert_eq!(flow(&ws, "abs.flows", "principal_face", a, day(5)), 0.0);
}

// ------------------------------------------------------------------------------------------- F06

#[test]
fn f06_principal_cash_is_15_not_17_and_the_loss_writes_down_the_junior() {
    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let cal = calendar(2);
    // 5 scheduled and 10 prepaid: 15 of principal cash; 2 written down without cash.
    let (c, ids) = pool(&mut ws, &cal, &[loan("L", 100.0, vec![(1, 0.0, 0.15, 0.02, 0.0)])]);
    let a = debt(&mut ws, "A", 80.0, 0.0);
    let b = debt(&mut ws, "B", 20.0, 0.0);
    let (ae, be) = (a.entity, b.entity);
    let mut d = deal(&mut ws, "abs", c, vec![(ids[0], 100.0)], vec![a, b], vec![day(1)]);
    d.losses = Losses::WriteDown { order: vec![1, 0], write_up: false };
    d.principal = vec![sequential(&[0, 1])];
    evaluate(&mut ws, &cal, vec![d]).unwrap();
    let t = day(1);
    let deal = ws.entities.get("deal", "abs").unwrap();
    assert_close!(at(&ws, "abs.collateral", "principal", deal, t), 15.0);
    assert_close!(at(&ws, "abs.collateral", "par", deal, t), 83.0);
    assert_close!(flow(&ws, "abs.distributions", "principal", ae, t), 15.0);
    assert_close!(flow(&ws, "abs.flows", "writedown_face", be, t) * 20.0, 2.0);
    assert_close!(at(&ws, "abs.marks", "balance", ae, t) + at(&ws, "abs.marks", "balance", be, t), 83.0);
}

#[test]
fn f06_the_deficiency_ledger_reserve_and_recoveries_are_tracked_apart_from_principal() {
    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let cal = calendar(4);
    // t1: 15 repaid, 2 lost, 4 of interest. t2: a recovery of 1 (cash with no fall in par),
    // 4 of interest. t3: only 0.2 of interest.
    let flows = vec![(1, 0.04, 0.15, 0.02, 0.0), (2, 0.04, 0.01, 0.0, 0.01), (3, 0.002, 0.0, 0.0, 0.0)];
    let (c, ids) = pool(&mut ws, &cal, &[loan("L", 100.0, flows)]);
    let a = debt(&mut ws, "A", 80.0, 0.04);
    let mut b = debt(&mut ws, "B", 20.0, 0.08);
    b.shortfall = Shortfall::Defer { compound: false };
    let e = equity(&mut ws, "E", 1.0);
    let (ae, be, ee) = (a.entity, b.entity, e.entity);
    let mut d = deal(&mut ws, "abs", c, vec![(ids[0], 100.0)], vec![a, b, e], (1..4).map(day).collect());
    d.losses = Losses::Ledger(vec![1, 0]);
    d.reserve = 0.5;
    d.reserve_target = 0.5;
    d.interest = vec![
        step(Action::Interest { tranches: vec![0], reserve: true }),
        interest(&[1]),
        step(Action::Deficiency(vec![0, 1])),
        step(Action::Reserve),
        step(Action::Residual(2)),
    ];
    d.principal = vec![sequential(&[0, 1])];
    evaluate(&mut ws, &cal, vec![d]).unwrap();
    let (t1, t2, t3) = (day(1), day(2), day(3));
    let deal = ws.entities.get("deal", "abs").unwrap();
    let reserve = ws.entities.get("account", "abs.reserve").unwrap();
    // t1: the 2 lost is debited to B's ledger, then cured from interest: A receives the 15 of
    // principal cash plus the 2 of cure, and the notes again equal the pool.
    assert_close!(flow(&ws, "abs.distributions", "principal", ae, t1), 17.0);
    assert_eq!(at(&ws, "abs.marks", "deficiency", be, t1), 0.0);
    assert_close!(flow(&ws, "abs.flows", "interest", ee, t1), 4.0 - 0.8 - 0.4 - 2.0);
    assert_close!(at(&ws, "abs.marks", "balance", ae, t1) + at(&ws, "abs.marks", "balance", be, t1), at(&ws, "abs.collateral", "par", deal, t1));
    // t2: the recovery is cash, paid as principal; it reinstates nothing.
    assert_close!(flow(&ws, "abs.distributions", "principal", ae, t2), 1.0);
    assert_eq!(flow(&ws, "abs.flows", "writeup_face", ae, t2), 0.0);
    // t3: A's interest (62 × 1%) is paid from 0.2 of interest and 0.42 drawn from the reserve,
    // which nothing is left to replenish; B's interest is deferred.
    assert_close!(flow(&ws, "abs.flows", "interest", ae, t3) * 80.0, 0.62);
    assert_close!(at(&ws, "abs.accounts", "balance", reserve, t3), 0.08);
    assert_close!(at(&ws, "abs.marks", "deferred", be, t3), 0.4);
}

// ------------------------------------------------------------------------------------------- F14

#[test]
fn f14_pac_support_exhaustion_and_interest_only_semantics() {
    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let cal = calendar(6);
    let principal = [5.0, 20.0, 20.0, 20.0, 10.0];
    let flows = principal.iter().enumerate().map(|(i, p)| (i + 1, 0.015, p / 100.0, 0.0, 0.0)).collect();
    let (c, ids) = pool(&mut ws, &cal, &[loan("M", 100.0, flows)]);
    let mut pac = debt(&mut ws, "PAC", 50.0, 0.05);
    pac.schedule = (1..6).map(|k| (day(k), 50.0 - 5.0 * k as f64)).collect();
    let sup = debt(&mut ws, "SUP", 30.0, 0.06);
    let po = Tranche::debt("PO", tranche(&mut ws, "PO"), 20.0, Coupon::None);
    let mut io = Tranche::debt("IO", tranche(&mut ws, "IO"), 50.0, Coupon::Fixed(0.01));
    io.claim = Claim::InterestOnly { of: Notional::Tranche(0), ratio: 1.0 };
    let e = equity(&mut ws, "E", 1.0);
    let [pe, se, poe, ioe, ee] = [&pac, &sup, &po, &io, &e].map(|t| t.entity);
    let mut d = deal(&mut ws, "cmo", c, vec![(ids[0], 100.0)], vec![pac, sup, po, io, e], (1..6).map(day).collect());
    d.interest = vec![interest(&[0, 1, 3]), step(Action::Residual(4))];
    d.principal = vec![step(Action::Principal { tranches: vec![0], pro_rata: false, schedule: true }), sequential(&[1]), sequential(&[0]), sequential(&[2])];
    evaluate(&mut ws, &cal, vec![d]).unwrap();
    let bal = |e: EntityId, k: i64| at(&ws, "cmo.marks", "balance", e, day(k));
    // Within the band the PAC follows its schedule and the support absorbs the rest.
    assert_close!(bal(pe, 1), 45.0);
    assert_close!(bal(pe, 2), 40.0);
    assert_close!(bal(se, 2), 15.0);
    // The support is exhausted at t3; after that the PAC takes all principal, below schedule.
    assert_close!(bal(se, 3), 0.0);
    assert_close!(bal(pe, 4), 15.0);
    assert_close!(bal(pe, 5), 5.0);
    assert_close!(bal(poe, 5), 20.0);
    // The IO's notional follows the PAC: its fall is written down, and never paid as principal.
    assert_close!(bal(ioe, 5), 5.0);
    assert_eq!(total(&ws, "cmo.flows", "principal_face", ioe), 0.0);
    assert_close!(total(&ws, "cmo.flows", "writedown_face", ioe), 0.9);
    assert_eq!(total(&ws, "cmo.flows", "interest", poe), 0.0);
    // Every claim's distributions reconcile to the collateral's cash.
    let deal = ws.entities.get("deal", "cmo").unwrap();
    for k in 1..6 {
        let t = day(k);
        let paid: f64 =
            [pe, se, poe, ioe, ee].iter().map(|r| flow(&ws, "cmo.distributions", "interest", *r, t) + flow(&ws, "cmo.distributions", "principal", *r, t)).sum();
        assert_close!(paid, at(&ws, "cmo.collateral", "interest", deal, t) + at(&ws, "cmo.collateral", "principal", deal, t));
    }
}

// ------------------------------------------------------------------------------------------- F18

#[test]
fn f18_a_loss_trigger_switches_pro_rata_to_sequential_and_keeps_balances() {
    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let cal = calendar(4);
    let (c, ids) = pool(&mut ws, &cal, &[loan("R", 100.0, vec![(1, 0.0, 0.1, 0.0, 0.0), (2, 0.0, 0.1, 0.03, 0.0), (3, 0.0, 0.1, 0.0, 0.0)])]);
    let tr = [("A", 80.0), ("M", 15.0), ("J", 5.0)].map(|(n, f)| debt(&mut ws, n, f, 0.0));
    let [a, m, j] = [&tr[0], &tr[1], &tr[2]].map(|t| t.entity);
    let mut d = deal(&mut ws, "srt", c, vec![(ids[0], 100.0)], tr.to_vec(), (1..4).map(day).collect());
    let te = ws.entities.intern("trigger", "sequential");
    d.triggers = vec![Trigger {
        name: "sequential".into(),
        entity: te,
        metric: Metric::CumulativeLoss,
        above: Some(0.02),
        below: None,
        lookback: 1,
        curable: false,
        effect: Effect::None,
    }];
    d.losses = Losses::WriteDown { order: vec![2, 1, 0], write_up: false };
    d.principal = vec![
        Step { action: Action::Principal { tranches: vec![0, 1, 2], pro_rata: true, schedule: false }, when: When::NotTripped(0) },
        Step { action: Action::Principal { tranches: vec![0, 1, 2], pro_rata: false, schedule: false }, when: When::Tripped(0) },
    ];
    evaluate(&mut ws, &cal, vec![d]).unwrap();
    let bal = |e: EntityId, k: i64| at(&ws, "srt.marks", "balance", e, day(k));
    assert_close!(bal(a, 1), 72.0);
    assert_close!(bal(m, 1), 13.5);
    assert_close!(bal(j, 1), 4.5);
    // The 3 lost writes down the junior; the trigger trips, and principal is sequential from then.
    assert_close!(bal(j, 2), 1.5);
    assert_close!(bal(a, 2), 62.0);
    assert_close!(bal(m, 2), 13.5);
    assert_close!(bal(a, 3), 52.0);
    // The claims still add to the pool, which a pool-factor scaling would not reproduce.
    let deal = ws.entities.get("deal", "srt").unwrap();
    assert_close!(bal(a, 3) + bal(m, 3) + bal(j, 3), at(&ws, "srt.collateral", "par", deal, day(3)));
    assert!((bal(j, 3) - 5.0 * 0.67).abs() > 1.0);
}

// -------------------------------------------------------------------------------------------- R9

#[test]
fn r9_a_coverage_breach_diverts_interest_until_it_cures_and_distributions_resume() {
    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let cal = calendar(5);
    let paying = |k: usize| (k, 0.02, 0.0, 0.0, 0.0);
    let loans = [loan("L1", 100.0, (1..5).map(paying).collect()), loan("L2", 100.0, vec![paying(1), (2, 0.02, 0.0, 0.25, 0.0), paying(3), paying(4)])];
    let (c, ids) = pool(&mut ws, &cal, &loans);
    let a = debt(&mut ws, "A", 80.0, 0.04);
    let e = equity(&mut ws, "E", 20.0);
    let (ae, ee) = (a.entity, e.entity);
    let mut d = deal(&mut ws, "clo", c, vec![(ids[0], 60.0), (ids[1], 40.0)], vec![a, e], (1..5).map(day).collect());
    let t = oc(&mut ws, "A_OC", &[0], 1.15, Cure::Paydown(vec![0]));
    let te = t.entity;
    d.tests = vec![t];
    d.interest = vec![interest(&[0]), step(Action::Test(0)), step(Action::Residual(1))];
    evaluate(&mut ws, &cal, vec![d]).unwrap();
    let eq = |k: i64| flow(&ws, "clo.flows", "interest", ee, day(k)) * 20.0;
    assert_close!(eq(1), 2.0 - 0.8);
    // t2: 10 of par lost, the test fails (90 / 80) and all 1.2 left after A's interest is diverted.
    assert_eq!(at(&ws, "clo.tests", "passed", te, day(2)), 0.0);
    assert_close!(flow(&ws, "clo.flows", "principal_face", ae, day(2)) * 80.0, 1.2);
    assert_eq!(eq(2), 0.0);
    // t3: still failing, but the cure needs only part of the cash: distributions resume.
    let cure = 78.8 - 90.0 / 1.15;
    assert_close!(at(&ws, "clo.tests", "cure", te, day(3)), cure);
    assert_close!(eq(3), 2.0 - 0.788 - cure);
    // t4: exactly at the threshold, the test passes and nothing is diverted.
    assert_eq!(at(&ws, "clo.tests", "passed", te, day(4)), 1.0);
    assert_eq!(at(&ws, "clo.tests", "cure", te, day(4)), 0.0);
}

// ---------------------------------------------------------------------- tranches in other books

#[test]
fn a_tranche_is_a_bond_another_book_holds_and_its_identity_holds() {
    let F02 { ws, a, e, .. } = f02();
    let cal = calendar(4);
    let bond = |entity| {
        Arc::new(SuppliedBond {
            entity,
            marks: "clo.marks".into(),
            clean: "clean".into(),
            accrued: "accrued".into(),
            flows: Some("clo.flows".into()),
            factor: Some("factor".into()),
            reconcile: true,
        }) as Arc<dyn Contract>
    };
    let (rec, _) = UnitRecords::compute(&[bond(a), bond(e)], &ws, &cal, MissingPolicy::Fail).unwrap();
    let cfg = LedgerConfig { initial_cash: 0.0, opening: vec![(a, 10.0), (e, 5.0)], ..LedgerConfig::default() };
    let out = Ledger::build(&ws, &rec, vec![], ExecRef::Fill(&ResearchFill::default()), cfg).unwrap().run().unwrap();
    assert!(out.max_identity_error <= 1e-9);
    let income: f64 = out.income.iter().sum();
    let expected = 10.0 * total(&ws, "clo.flows", "interest", a) + 5.0 * total(&ws, "clo.flows", "interest", e);
    assert_close!(income, expected);
    // The equity is marked at book value: the collateral and accounts less the debt's claims.
    assert_close!(out.nav[0], 10.0 + 5.0);
}

#[test]
fn a_deal_restarted_from_a_checkpoint_matches_a_full_run() {
    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let cal = calendar(7);
    let times = cal.times().to_vec();
    let (c, ids) = pool(&mut ws, &cal, &[loan("R", 100.0, (1..6).map(|k| (k, 0.015, 0.18, 0.01, 0.0)).collect()), loan("N", 100.0, vec![])]);
    let a = debt(&mut ws, "A", 80.0, 0.01);
    let e = equity(&mut ws, "E", 20.0);
    let mut d = deal(&mut ws, "abs", c.clone(), vec![(ids[0], 100.0)], vec![a, e], (1..7).map(day).collect());
    d.reinvest_until = Some(day(3));
    d.losses = Losses::Ledger(vec![1, 0]);
    d.interest = vec![interest(&[0]), step(Action::Deficiency(vec![0, 1])), step(Action::Residual(1))];
    d.principal = vec![Step { action: Action::Principal { tranches: vec![0], pro_rata: false, schedule: false }, when: When::Amortising }];
    d.manager = vec![Manager { name: "replenish".into(), decide: Arc::new(Reinvest { into: vec![ids[1]], keep: 0.0 }), schedule: times, lag: 0 }];
    let (rec, _) = UnitRecords::compute(&c, &ws, &cal, MissingPolicy::Fail).unwrap();
    let full = d.book(&ws, &rec).unwrap().run().unwrap();
    let mut first = d.book(&ws, &rec).unwrap();
    for _ in 0..3 {
        first.step().unwrap();
    }
    let saved = first.checkpoint();
    let mut resumed = d.book(&ws, &rec).unwrap();
    resumed.restore(saved);
    while resumed.step().unwrap() {}
    let resumed = resumed.finish();
    assert_eq!(resumed.nav, full.nav);
    assert_eq!(resumed.capital, full.capital);
    assert!(full.capital.iter().any(|c| *c < 0.0));
}

#[test]
fn a_repayment_larger_than_the_balance_is_refused() {
    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let cal = calendar(3);
    let (c, _) = pool(&mut ws, &cal, &[loan("L", 100.0, vec![(1, 0.0, 0.7, 0.0, 0.0), (2, 0.0, 0.5, 0.0, 0.0)])]);
    let err = UnitRecords::compute(&c, &ws, &cal, MissingPolicy::Fail).err().unwrap().to_string();
    assert!(err.contains("L: the flows") && err.contains("outstanding"), "{err}");
}

// ------------------------------------------------------------- one producer per state (change 4)

/// The F02 deal and a trustee's report of its tranche flows, which differs on A's first coupon.
fn trustee(ws: &mut Workspace) {
    let a = ws.entities.get("tranche", "A").unwrap();
    let cols = ["interest", "principal_face", "writedown_face", "writeup_face", "pik_face"]
        .iter()
        .map(|c| Column::new(*c, Unit::Number, vec![if *c == "interest" { 1.39 / 70.0 } else { 0.0 }]))
        .collect();
    ws.add_series(Series::rows("trustee", vec![a], vec![day(1)], vec![day(1)], cols).unwrap()).unwrap();
}

fn f02_deal(ws: &mut Workspace, cal: &Calendar) -> Deal {
    let (c, ids) = pool(ws, cal, &[loan("L", 100.0, vec![(1, 0.025, 0.0, 0.0, 0.0), (2, 0.016, 0.0, 0.0, 0.0), (3, 0.035, 0.0, 0.0, 0.0)])]);
    let a = debt(ws, "A", 70.0, 0.08);
    let e = equity(ws, "E", 30.0);
    let mut d = deal(ws, "clo", c, vec![(ids[0], 100.0)], vec![a, e], vec![day(1), day(2), day(3)]);
    d.interest = vec![interest(&[0]), step(Action::Residual(1))];
    d
}

#[test]
fn an_observation_stands_in_for_a_derived_state_when_nothing_upstream_is_intervened() {
    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let cal = calendar(4);
    let d = f02_deal(&mut ws, &cal);
    trustee(&mut ws);
    let mut g = Graph::new();
    g.add(Model::batch(d)).substitute("clo.flows", "trustee");
    let report = g.evaluate(&mut ws, &cal).unwrap();
    assert_eq!(report.substituted, vec![("clo.flows".to_owned(), "trustee".to_owned())]);
    assert_eq!(report.mode(), Mode::Replay);
    let a = ws.entities.get("tranche", "A").unwrap();
    assert_close!(flow(&ws, "clo.flows", "interest", a, day(1)) * 70.0, 1.39);
    let p = ws.series("clo.flows").unwrap().provenance().clone();
    assert_eq!(p.producer, Producer::Observed);
    assert_eq!(p.describes.as_deref(), Some("clo.flows"));
    // The deal's other states are still its own.
    assert!(at(&ws, "clo.marks", "balance", a, day(1)).is_finite());
}

#[test]
fn f03_r10_p6_a_counterfactual_refuses_the_real_deals_reported_flows() {
    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let cal = calendar(5);
    let paying = |k: usize| (k, 0.02, 0.0, 0.0, 0.0);
    let loans = [
        loan("L1", 100.0, (1..5).map(paying).collect()),
        loan("L2", 100.0, (1..5).map(paying).collect()),
        loan("L3", 100.0, (1..5).map(paying).collect()),
        loan("L4", 100.0, vec![paying(1), paying(2), (3, 0.0, 0.0, 0.5, 0.0), (4, 0.0, 0.0, 0.0, 0.0)]),
    ];
    let (c, ids) = pool(&mut ws, &cal, &loans);
    let t2 = day(2);
    for (name, buy) in [("trades", ids[2]), ("alt", ids[3])] {
        let cols = vec![Column::new("qty", Unit::Number, vec![-50.0, 50.0])];
        ws.add_series(Series::rows(name, vec![ids[1], buy], vec![t2, t2], vec![t2, t2], cols).unwrap()).unwrap();
    }
    let a = ws.entities.intern("tranche", "A");
    let cols = ["interest", "principal_face"].iter().map(|c| Column::new(*c, Unit::Number, vec![0.01])).collect();
    ws.add_series(Series::rows("trustee", vec![a], vec![day(1)], vec![day(1)], cols).unwrap()).unwrap();
    let trades = |s: &str| Arc::new(Trades { series: s.into(), column: "qty".into(), universe: ids.clone() }) as Arc<dyn DecisionModel>;
    // The changed trade makes the deal a counterfactual: its states are derived, so the real
    // deal's reported flows cannot stand in for them.
    let cf = f03(&mut ws, &cal, "cf", Arc::new(Branch { at: t2, before: trades("trades"), after: trades("alt") }), &c, &ids);
    let mut g = Graph::new();
    g.add(Model::batch(cf.clone())).substitute("cf.flows", "trustee");
    let err = g.evaluate(&mut ws, &cal).err().unwrap().to_string();
    assert!(err.contains("counterfactual") && err.contains("cf's manager"), "{err}");
    // Without the substitution, it runs, and everything downstream carries the intervention.
    let mut g = Graph::new();
    g.add(Model::batch(cf));
    let report = g.evaluate(&mut ws, &cal).unwrap();
    assert_eq!(report.mode(), Mode::Counterfactual);
    assert!(matches!(&ws.series("cf.flows").unwrap().provenance().intervention, Some(Intervention::Counterfactual(by)) if by.contains("cf's manager")));
    // A branch with no change is the baseline: the trustee's report may replay it.
    let same = f03(&mut ws, &cal, "same", Arc::new(Branch { at: t2, before: trades("trades"), after: trades("trades") }), &c, &ids);
    let mut g = Graph::new();
    g.add(Model::batch(same)).substitute("same.flows", "trustee");
    assert_eq!(g.evaluate(&mut ws, &cal).unwrap().mode(), Mode::Replay);
    // Trades from a scenario series are an intervention too, however they are applied.
    let mut ws2 = ws.clone();
    let cols = vec![Column::new("qty", Unit::Number, vec![-50.0, 50.0])];
    let scenario = Series::rows("plan", vec![ids[1], ids[3]], vec![t2, t2], vec![t2, t2], cols).unwrap();
    let p = Provenance { intervention: Some(Intervention::Scenario("the manager's plan".into())), ..scenario.provenance().clone() };
    ws2.add_series(scenario.with_provenance(p)).unwrap();
    let planned = f03(&mut ws2, &cal, "planned", trades("plan"), &c, &ids);
    let mut g = Graph::new();
    g.add(Model::batch(planned));
    assert_eq!(g.evaluate(&mut ws2, &cal).unwrap().mode(), Mode::Counterfactual);
}

#[test]
fn a_stress_propagates_to_everything_downstream() {
    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let cal = calendar(3);
    let times = cal.times().to_vec();
    let bonds: Vec<EntityId> = ["X", "Y", "Z"].iter().map(|n| ws.entities.intern("bond", n)).collect();
    let v: Vec<f64> = (0..times.len() * 3).map(|i| i as f64).collect();
    ws.add_series(Series::grid("spreads", bonds, times.clone(), times, vec![Column::new("s", Unit::Rate, v)]).unwrap()).unwrap();
    let z = |name: &str, input: Input| ZScore { name: name.into(), input, group: None, ddof: 0.0, min_count: 2 };
    let shock = Model::cross_section(z("shock", Input::exact("spreads", "s"))).amend(|s| ModelSpec { role: ModelRole::Stress, ..s });
    let signal = Model::cross_section(z("signal", Input::exact("shock", "z")));
    let mut g = Graph::new();
    g.add(shock).add(signal);
    let report = g.evaluate(&mut ws, &cal).unwrap();
    assert_eq!(report.mode(), Mode::Scenario);
    assert_eq!(ws.series("signal").unwrap().provenance().intervention, Some(Intervention::Scenario("shock".into())));
    // A signal can still be substituted where its inputs are observed, but not downstream of the stress.
    let mut g = Graph::new();
    g.add(Model::cross_section(z("shock2", Input::exact("spreads", "s"))).amend(|s| ModelSpec { role: ModelRole::Stress, ..s }))
        .add(Model::cross_section(z("signal2", Input::exact("shock2", "z"))))
        .substitute("signal2", "spreads");
    assert!(g.evaluate(&mut ws, &cal).err().unwrap().to_string().contains("scenario by shock2"));
}

#[test]
fn a_runs_mode_follows_its_decisions_and_its_world() {
    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let cal = calendar(4);
    let d = f02_deal(&mut ws, &cal);
    evaluate(&mut ws, &cal, vec![d]).unwrap();
    let a = ws.entities.get("tranche", "A").unwrap();
    let bond = Arc::new(SuppliedBond {
        entity: a,
        marks: "clo.marks".into(),
        clean: "clean".into(),
        accrued: "accrued".into(),
        flows: Some("clo.flows".into()),
        factor: Some("factor".into()),
        reconcile: true,
    }) as Arc<dyn Contract>;
    let reads = bond.reads();
    let (rec, _) = UnitRecords::compute(&[bond], &ws, &cal, MissingPolicy::Fail).unwrap();
    let held = Series::rows("held", vec![a], vec![day(0)], vec![day(0)], vec![Column::new("qty", Unit::Number, vec![10.0])]).unwrap();
    ws.add_series(held).unwrap();
    let book = |decide: Arc<dyn DecisionModel>| Book {
        name: "b".into(),
        policies: vec![PolicySpec { name: "p".into(), decide, schedule: vec![day(0)], scope: None, priority: 0, lag: 0, time_in_force: None }],
        execution: Executor::Fill(Arc::new(ResearchFill::default())),
        config: LedgerConfig { initial_cash: 10.0, ..LedgerConfig::default() },
    };
    let replay = book(Arc::new(Replay { series: "held".into(), column: "qty".into(), universe: vec![a], quantities: true }));
    assert_eq!(run_mode(&ws, &reads, &replay), RunMode::HoldingsReplay);
    let top = book(Arc::new(TopN { series: "clo.marks".into(), column: "clean".into(), universe: vec![a], n: 1, gross: 1.0 }));
    assert_eq!(run_mode(&ws, &reads, &top), RunMode::PolicyBacktest);
    // P2, E4: the replay's NAV against an official one, which misses A's last coupon. The
    // difference is reported, not plugged.
    let out = run_book(&ws, &rec, &replay).unwrap();
    let mut official = out.nav.clone();
    let last = official.len() - 1;
    official[last] -= 10.0 * flow(&ws, "clo.flows", "interest", a, day(3));
    let n = official.len();
    ws.add_series(
        Series::rows("admin", vec![EntityId::NONE; n], out.times.clone(), out.times.clone(), vec![Column::new("nav", Unit::Number, official)]).unwrap(),
    )
    .unwrap();
    let r = reconcile(&ws, &out, "admin", "nav", EntityId::NONE).unwrap();
    assert_close!(r.worst(), 10.0 * flow(&ws, "clo.flows", "interest", a, day(3)));
    assert_eq!(r.difference[..last].iter().map(|d| d.abs()).sum::<f64>(), 0.0);
}

// ------------------------------------------------------- contractual rounding (spec I1, F01 note)

/// A deal paying whole cents: collateral `par`, senior debt 200, a 104% par test cured from
/// `interest`.
fn in_cents(par: f64, interest: f64) -> (Workspace, EntityId, EntityId, EntityId) {
    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let cal = calendar(2);
    let (c, ids) = pool(&mut ws, &cal, &[loan("L", 100.0, vec![(1, interest / par, 0.0, 0.0, 0.0)])]);
    let a = debt(&mut ws, "A", 200.0, 0.0);
    let e = equity(&mut ws, "E", 1.0);
    let (ae, ee) = (a.entity, e.entity);
    let mut d = deal(&mut ws, "clo", c, vec![(ids[0], par)], vec![a, e], vec![day(1)]);
    let t = oc(&mut ws, "A_OC", &[0], 1.04, Cure::Paydown(vec![0]));
    let te = t.entity;
    d.tests = vec![t];
    d.interest = vec![step(Action::Test(0)), step(Action::Residual(1))];
    d.rounding = Some(Rounding::cents(RoundingMode::HalfUp));
    evaluate(&mut ws, &cal, vec![d]).unwrap();
    (ws, ae, ee, te)
}

#[test]
fn in_whole_cents_a_cure_is_the_smallest_whole_cent_that_cures() {
    // Exactly, the cure is 200 − 205 / 1.04 = 2.8846…; in cents, the largest debt that passes is
    // 197.11, so 2.89 is diverted and the test passes after it. 2.88 would leave it failing.
    let (ws, a, e, test) = in_cents(205.0, 5.0);
    let t = day(1);
    assert_close!(at(&ws, "clo.tests", "cure", test, t), 2.89);
    assert_eq!(at(&ws, "clo.tests", "passed_after", test, t), 1.0);
    let left = at(&ws, "clo.marks", "balance", a, t);
    assert_close!(left, 197.11);
    assert_close!(flow(&ws, "clo.distributions", "interest", e, t), 5.0 - 2.89);
    assert!(205.0 / (left + 0.01) < 1.04, "one cent less does not cure");
    // With 2.00 of interest the whole of it is diverted, and the test still fails.
    let (ws, a, e, test) = in_cents(205.0, 2.0);
    assert_close!(at(&ws, "clo.tests", "cure", test, t), 2.0);
    assert_eq!(at(&ws, "clo.tests", "passed_after", test, t), 0.0);
    assert_close!(at(&ws, "clo.marks", "balance", a, t), 198.0);
    assert_eq!(flow(&ws, "clo.distributions", "interest", e, t), 0.0);
}

#[test]
fn in_whole_cents_a_pro_rata_split_gives_the_odd_cent_to_the_largest_remainder() {
    // Two classes are each owed 0.50 and 0.75 is available: 0.375 each exactly. In cents, the
    // earlier class takes the odd cent and the rest stays owed.
    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let cal = calendar(2);
    let (c, ids) = pool(&mut ws, &cal, &[loan("L", 100.0, vec![(1, 0.0075, 0.0, 0.0, 0.0)])]);
    let a = debt(&mut ws, "A", 100.0, 0.02);
    let b = debt(&mut ws, "B", 100.0, 0.02);
    let (ae, be) = (a.entity, b.entity);
    let mut d = deal(&mut ws, "clo", c, vec![(ids[0], 100.0)], vec![a, b], vec![day(1)]);
    d.interest = vec![interest(&[0, 1])];
    d.rounding = Some(Rounding::cents(RoundingMode::HalfUp));
    evaluate(&mut ws, &cal, vec![d]).unwrap();
    let t = day(1);
    assert_close!(flow(&ws, "clo.distributions", "interest", ae, t), 0.38);
    assert_close!(flow(&ws, "clo.distributions", "interest", be, t), 0.37);
    assert_close!(at(&ws, "clo.marks", "deferred", ae, t), 0.12);
    assert_close!(at(&ws, "clo.marks", "deferred", be, t), 0.13);
}
