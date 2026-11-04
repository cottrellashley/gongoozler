//! Financing and collateral (spec, change 5): agreements are contracts between the book and a
//! counterparty, with their own state. A higher haircut is a funding event, not a loss (R8);
//! eligibility, concentration and encumbrance set what collateral supports (F20, R14); funding
//! does not extend itself (F21); a cash need is met by the declared responses, or the book
//! fails explicitly (R7, P9); annexes collateralise each netting set apart (F22).

use std::sync::Arc;

use fi_kernel::{
    Advance, Agreement, AgreementKind, Basis, Calendar, CapitalFlows, Column, Contract, DecisionCtx, DecisionModel, EntityId, ExecRef, KnowledgePolicy, Ledger,
    LedgerConfig, LedgerOutput, MissingPolicy, OpeningDue, Policy, ResearchFill, Response, Rounding, RoundingMode, Series, Settlement, Targets, Ts, Unit,
    UnitRecords, Workspace,
};
use fi_std::{SuppliedBond, TotalReturn};

fn day(k: i64) -> Ts {
    Ts::from_days(19_000 + k)
}

fn calendar(n: i64) -> Calendar {
    Calendar::new((0..n).map(day).collect())
}

/// Bonds marked at constant clean prices, one unit per unit of face.
fn bonds(ws: &mut Workspace, cal: &Calendar, names: &[(&str, f64)]) -> (Vec<Arc<dyn Contract>>, Vec<EntityId>) {
    let times = cal.times().to_vec();
    let ids: Vec<EntityId> = names.iter().map(|(n, _)| ws.entities.intern("bond", n)).collect();
    let clean = (0..times.len()).flat_map(|_| names.iter().map(|(_, p)| *p)).collect();
    let cols = vec![Column::new("clean", Unit::Price, clean), Column::new("accrued", Unit::Price, vec![0.0; times.len() * names.len()])];
    ws.add_series(Series::grid("marks", ids.clone(), times.clone(), times, cols).unwrap()).unwrap();
    let c = ids
        .iter()
        .map(|&entity| {
            Arc::new(SuppliedBond {
                entity,
                marks: "marks".into(),
                clean: "clean".into(),
                accrued: "accrued".into(),
                flows: None,
                factor: None,
                reconcile: true,
            }) as Arc<dyn Contract>
        })
        .collect();
    (c, ids)
}

/// A per-entity rows series of one column.
fn rows(ws: &mut Workspace, name: &str, column: &str, data: &[(EntityId, Ts, f64)]) {
    let s = Series::rows(
        name,
        data.iter().map(|r| r.0).collect(),
        data.iter().map(|r| r.1).collect(),
        data.iter().map(|r| r.1).collect(),
        vec![Column::new(column, Unit::Number, data.iter().map(|r| r.2).collect())],
    )
    .unwrap();
    ws.add_series(s).unwrap();
}

#[allow(clippy::too_many_arguments)]
fn secured(
    name: &str,
    collateral: Vec<(EntityId, Advance)>,
    basis: Basis,
    concentration: Option<f64>,
    limit: f64,
    maturity: Option<Ts>,
    cure: usize,
    opening: f64,
    draw: bool,
    sweep: bool,
) -> Agreement {
    Agreement {
        name: name.into(),
        kind: AgreementKind::Secured {
            collateral,
            basis,
            concentration,
            limit,
            rate: None,
            maturity,
            cure,
            opening,
            draw,
            sweep,
            margin: None,
            rounding: None,
        },
    }
}

/// Holds what it opened with: no decisions.
fn hold(ws: &Workspace, rec: &UnitRecords, cfg: LedgerConfig) -> LedgerOutput {
    Ledger::build(ws, rec, vec![], ExecRef::Fill(&ResearchFill::default()), cfg).unwrap().run().unwrap()
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * b.abs().max(1.0)
}

macro_rules! assert_close {
    ($a:expr, $b:expr) => {{
        let (a, b) = ($a, $b);
        assert!(close(a, b), "{} = {a}, expected {b}", stringify!($a));
    }};
}

fn repo_book(cash: f64, responses: Vec<Response>) -> (LedgerOutput, Workspace) {
    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let cal = calendar(6);
    let (c, ids) = bonds(&mut ws, &cal, &[("B", 100.0)]);
    // The haircut rises from 5% to 10% at t3.
    rows(&mut ws, "haircuts", "advance", &[(ids[0], day(0), 0.95), (ids[0], day(3), 0.90)]);
    let (rec, _) = UnitRecords::compute(&c, &ws, &cal, MissingPolicy::Fail).unwrap();
    let repo = secured(
        "repo",
        vec![(ids[0], Advance::Series("haircuts".into(), "advance".into()))],
        Basis::Market,
        None,
        f64::INFINITY,
        None,
        1,
        95_000.0,
        false,
        false,
    );
    let cfg =
        LedgerConfig { initial_cash: cash, opening: vec![(ids[0], 100_000.0)], financing: vec![repo], responses, journal: true, ..LedgerConfig::default() };
    (hold(&ws, &rec, cfg), ws)
}

#[test]
fn r8_a_haircut_increase_is_a_funding_event_not_a_loss() {
    let (out, _) = repo_book(5_000.0, vec![Response::Convert, Response::Draw]);
    let p = &out.agreements[0];
    // NAV is collateral plus cash less the loan: the haircut moves none of them.
    assert!(out.nav.iter().all(|n| close(*n, 10_000.0)), "{:?}", out.nav);
    assert_close!(p.capacity[2], 95_000.0);
    assert_close!(p.capacity[3], 90_000.0);
    // The call is made at t3 and met from cash a step later.
    assert_close!(p.call[3], 5_000.0);
    assert_close!(p.balance[4], 90_000.0);
    assert_close!(out.cash[4], 0.0);
    assert!(out.failed_at.is_none());
}

#[test]
fn r8_a_call_the_cash_cannot_meet_fails_the_book_unless_a_response_meets_it() {
    // Without a response that raises cash, the shortfall is an explicit failure.
    let (out, _) = repo_book(2_000.0, vec![Response::Convert, Response::Draw]);
    assert_eq!(out.failed_at, Some(day(4)));
    assert_close!(out.shortfall, 3_000.0);
    // Selling the collateral (here, all of it is pledged) repays the lender first: a closeout.
    let (out, _) = repo_book(2_000.0, vec![Response::Convert, Response::Draw, Response::Sell { cost_bp: 50.0 }]);
    assert!(out.failed_at.is_none());
    assert_close!(out.agreements[0].balance[4], 0.0);
    // 2,000 − 5,000 called + 99,500 of proceeds − 90,000 repaid; the 500 cost is the only loss.
    assert_close!(out.cash[4], 6_500.0);
    assert_close!(out.nav[4], 7_000.0 - 500.0);
    assert!(out.max_identity_error <= 1e-9);
}

#[test]
fn f20_r14_eligibility_concentration_and_encumbrance_set_the_borrowing_base() {
    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let cal = calendar(5);
    let (c, ids) = bonds(&mut ws, &cal, &[("L1", 98.0), ("L2", 95.0), ("L3", 90.0)]);
    // L3 stops being eligible at t2.
    rows(&mut ws, "advance", "rate", &[(ids[0], day(0), 0.8), (ids[1], day(0), 0.8), (ids[2], day(0), 0.8), (ids[2], day(2), 0.0)]);
    let (rec, _) = UnitRecords::compute(&c, &ws, &cal, MissingPolicy::Fail).unwrap();
    let adv = |i: usize| (ids[i], Advance::Series("advance".into(), "rate".into()));
    let w1 = secured("w1", vec![adv(0), adv(1), adv(2)], Basis::Par, Some(0.4), 70.0, None, 1, 60.0, false, false);
    // A second facility on L1, which the first already holds as collateral.
    let w2 = secured("w2", vec![adv(0)], Basis::Par, None, 100.0, None, 0, 0.0, true, false);
    let opening = vec![(ids[0], 40.0), (ids[1], 30.0), (ids[2], 30.0)];
    let cfg = LedgerConfig { initial_cash: 20.0, opening, financing: vec![w1, w2], ..LedgerConfig::default() };
    let out = hold(&ws, &rec, cfg);
    let (w1, w2) = (&out.agreements[0], &out.agreements[1]);
    // 80% of par: 32 + 24 + 24 = 80, within the 40% concentration cap, and limited to 70.
    assert_close!(w1.capacity[0], 70.0);
    // Once L3 is ineligible, 56 is eligible, and no holding may count above 22.4: L1 is cut from
    // 32. The deficiency is a call, cured in cash a step later.
    assert_close!(w1.capacity[2], 44.8);
    assert_close!(w1.call[2], 15.2);
    assert_close!(w1.balance[3], 44.8);
    assert_close!(out.cash[3], 20.0 - 15.2);
    // Loan principal, borrowing-base value and market value stay separate.
    assert_close!(w1.value[0], 100.0);
    assert_close!(w1.value[2], 70.0);
    assert_close!(out.nav[0], 40.0 * 0.98 + 30.0 * 0.95 + 30.0 * 0.90 + 20.0 - 60.0);
    // Pledged to w1, L1 supports nothing under w2.
    assert!(w2.capacity.iter().all(|c| *c == 0.0));
    assert!(out.failed_at.is_none());
}

fn warehouse(responses: Vec<Response>) -> LedgerOutput {
    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let cal = calendar(5);
    let (c, ids) = bonds(&mut ws, &cal, &[("L1", 100.0), ("L2", 100.0)]);
    let (rec, _) = UnitRecords::compute(&c, &ws, &cal, MissingPolicy::Fail).unwrap();
    let w =
        secured("warehouse", vec![(ids[0], Advance::Fixed(0.8)), (ids[1], Advance::Fixed(0.8))], Basis::Par, None, 100.0, Some(day(3)), 0, 80.0, false, false);
    let cfg = LedgerConfig { initial_cash: 5.0, opening: vec![(ids[0], 50.0), (ids[1], 50.0)], financing: vec![w], responses, ..LedgerConfig::default() };
    hold(&ws, &rec, cfg)
}

#[test]
fn f21_a_warehouse_at_maturity_is_repaid_or_the_failure_is_visible() {
    // No takeout: nothing extends the facility, and the deficit is a failure that stays in the
    // results.
    let out = warehouse(vec![Response::Convert, Response::Draw]);
    assert_eq!(out.failed_at, Some(day(3)));
    assert_close!(out.shortfall, 75.0);
    assert_eq!(out.nav.len(), 5);
    // A takeout at 95: the proceeds pay the facility and its costs before the residual equity.
    let out = warehouse(vec![Response::Sell { cost_bp: 500.0 }]);
    assert!(out.failed_at.is_none());
    assert_close!(out.agreements[0].balance[3], 0.0);
    assert_close!(out.cash[3], 5.0 + 95.0 - 80.0);
    assert_close!(out.nav[3], 25.0 - 5.0);
}

/// Sells B at `at`, whole.
struct SellB {
    b: EntityId,
    at: Ts,
}

impl DecisionModel for SellB {
    fn name(&self) -> String {
        "sell B".into()
    }
    fn params(&self) -> u64 {
        0
    }
    fn decide(&self, ctx: &DecisionCtx<'_>) -> fi_kernel::Result<Option<Targets>> {
        Ok((ctx.t == self.at).then(|| Targets::Orders(vec![(self.b, -100.0)])))
    }
}

fn redemption(financing: Vec<Agreement>) -> LedgerOutput {
    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let cal = calendar(5);
    let times = cal.times().to_vec();
    let (a, b) = (ws.entities.intern("index", "A"), ws.entities.intern("index", "B"));
    let lv = vec![100.0; times.len() * 2];
    ws.add_series(Series::grid("lv", vec![a, b], times.clone(), times, vec![Column::new("level", Unit::Price, lv)]).unwrap()).unwrap();
    rows(&mut ws, "flows", "amount", &[(EntityId::NONE, day(2), -5_000.0)]);
    let level = |e| Arc::new(TotalReturn { entity: e, series: "lv".into(), column: "level".into() }) as Arc<dyn Contract>;
    let (rec, _) = UnitRecords::compute(&[level(a), level(b)], &ws, &cal, MissingPolicy::Fail).unwrap();
    let decide = SellB { b, at: day(2) };
    let cfg = LedgerConfig {
        initial_cash: 0.0,
        opening: vec![(a, 100.0), (b, 100.0)],
        settlement: Settlement { lag: 1, by_contract: vec![] },
        flows: Some(CapitalFlows::Series { series: "flows".into(), column: "amount".into(), settle: 0 }),
        financing,
        ..LedgerConfig::default()
    };
    let p = Policy::new("sell", &decide, &[day(2)], 0);
    Ledger::build(&ws, &rec, vec![p], ExecRef::Fill(&ResearchFill::default()), cfg).unwrap().run().unwrap()
}

#[test]
fn r7_a_redemption_before_the_sale_settles_is_funded_or_detected() {
    // Without a facility, the redemption's cash is not there yet: a visible failure.
    let out = redemption(vec![]);
    assert_eq!(out.failed_at, Some(day(2)));
    assert_close!(out.shortfall, 5_000.0);
    // A facility on A bridges it, and the sale's proceeds repay it the step they settle.
    let a = EntityId(0);
    let bridge = secured("bridge", vec![(a, Advance::Fixed(0.5))], Basis::Market, None, f64::INFINITY, None, 0, 0.0, true, true);
    let out = redemption(vec![bridge]);
    assert!(out.failed_at.is_none(), "{:?}", out.failed_at);
    assert_close!(out.agreements[0].balance[2], 5_000.0);
    assert_close!(out.agreements[0].balance[3], 0.0);
    assert_close!(out.cash[3], 5_000.0);
}

fn swaps(cash: f64, financing: impl Fn(EntityId, EntityId, EntityId) -> Vec<Agreement>) -> LedgerOutput {
    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let cal = calendar(4);
    let times = cal.times().to_vec();
    let (s1, s2) = (ws.entities.intern("swap", "S1"), ws.entities.intern("swap", "S2"));
    // Mark-to-market per unit: S1 loses what S2 gains.
    let mtm = [0.0, 0.0, -10.0, 10.0, -30.0, 30.0, -20.0, 20.0].to_vec();
    ws.add_series(Series::grid("mtm", vec![s1, s2], times.clone(), times, vec![Column::new("value", Unit::Number, mtm)]).unwrap()).unwrap();
    let (bc, bid) = bonds(&mut ws, &cal, &[("G", 100.0)]);
    let swap = |e| Arc::new(TotalReturn { entity: e, series: "mtm".into(), column: "value".into() }) as Arc<dyn Contract>;
    let (rec, _) = UnitRecords::compute(&[swap(s1), swap(s2), bc[0].clone()], &ws, &cal, MissingPolicy::Fail).unwrap();
    let cfg = LedgerConfig {
        initial_cash: cash,
        opening: vec![(s1, 1.0), (s2, 1.0), (bid[0], 100.0)],
        financing: financing(s1, s2, bid[0]),
        ..LedgerConfig::default()
    };
    hold(&ws, &rec, cfg)
}

fn csa(name: &str, e: EntityId) -> Agreement {
    Agreement { name: name.into(), kind: AgreementKind::Csa { set: vec![e], threshold: 0.0, minimum: 0.0, every: 1 } }
}

#[test]
fn f22_p9_annexes_collateralise_each_netting_set_apart_and_margin_uses_cash() {
    let out = swaps(100.0, |s1, s2, _| vec![csa("csa1", s1), csa("csa2", s2)]);
    let (c1, c2) = (&out.agreements[0], &out.agreements[1]);
    // No netting across the two annexes: one posts, the other receives.
    assert_close!(c1.balance[1], 10.0);
    assert_close!(c2.balance[1], -10.0);
    assert_close!(c1.balance[2], 30.0);
    // Posting cash is a receivable, not a loss: NAV is what the book owns, unchanged.
    let start = 100.0 + 100.0;
    assert!(out.nav.iter().all(|n| close(*n, start)), "{:?}", out.nav);
    assert!(out.max_identity_error <= 1e-9);
    // One annex alone, with 15 of cash: the second call (20 more) is more than the cash. With no
    // facility the book fails; with one on its government bond, it draws.
    let out = swaps(15.0, |s1, _, _| vec![csa("csa1", s1)]);
    assert_eq!(out.failed_at, Some(day(2)));
    let out = swaps(15.0, |s1, _, g| {
        vec![csa("csa1", s1), secured("repo", vec![(g, Advance::Fixed(0.9))], Basis::Market, None, f64::INFINITY, None, 0, 0.0, true, false)]
    });
    assert!(out.failed_at.is_none());
    assert_close!(out.agreements[1].balance[2], 15.0);
}

// ------------------------------------- obligations paid from what is spendable (the report, §7)

/// A repo of `outstanding` on collateral worth 500,000, with margin assessed at `cut` at an 8%
/// haircut, free cash 4,000, and a receipt of 7,000 settling at `receipt`.
fn margin_call(times: &[Ts], cut: Ts, receipt: Ts, outstanding: f64) -> LedgerOutput {
    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let cal = Calendar::new(times.to_vec());
    let (c, ids) = bonds(&mut ws, &cal, &[("G", 100.0)]);
    let (rec, _) = UnitRecords::compute(&c, &ws, &cal, MissingPolicy::Fail).unwrap();
    let repo = Agreement {
        name: "repo".into(),
        kind: AgreementKind::Secured {
            collateral: vec![(ids[0], Advance::Fixed(0.92))],
            basis: Basis::Market,
            concentration: None,
            limit: f64::INFINITY,
            rate: None,
            maturity: None,
            cure: 0,
            opening: outstanding,
            draw: false,
            sweep: false,
            margin: Some(vec![cut]),
            rounding: Some(Rounding::cents(RoundingMode::Down)),
        },
    };
    let cfg = LedgerConfig {
        initial_cash: 4_000.0,
        opening: vec![(ids[0], 500_000.0)],
        opening_dues: vec![OpeningDue { currency: EntityId::NONE, amount: 7_000.0, settles_at: receipt }],
        financing: vec![repo],
        responses: vec![],
        journal: true,
        ..LedgerConfig::default()
    };
    hold(&ws, &rec, cfg)
}

#[test]
fn a_margin_call_is_paid_from_settled_cash_and_the_rest_stays_owed() {
    let cut = day(1);
    // The receipt settles after the cut: it cannot fund the call. 4,000 of the 10,000 call is
    // paid; 6,000 stays called and the loan is still 466,000. Cash does not go negative.
    let out = margin_call(&[day(0), cut], cut, cut.plus(5 * 60 * 1_000_000_000), 470_000.0);
    let p = &out.agreements[0];
    assert_eq!((p.capacity[1], p.called[1], p.paid[1], p.call[1], p.balance[1]), (460_000.0, 10_000.0, 4_000.0, 6_000.0, 466_000.0));
    assert_eq!(out.cash[1], 0.0);
    assert_eq!(out.overdue, vec![0.0, 6_000.0]);
    assert_eq!(out.failed_at, Some(cut));
    assert_close!(out.shortfall, 6_000.0);
    assert_close!(out.final_receivable, 7_000.0);
    // No call before the lender's margin time, though the haircut applies from the start.
    assert_eq!((p.called[0], p.paid[0]), (0.0, 0.0));
    // Settled before the cut, the receipt pays the call in full and 1,000 is left.
    let out = margin_call(&[day(0), cut], cut, day(0), 470_000.0);
    let p = &out.agreements[0];
    assert_eq!((p.paid[1], p.call[1], out.cash[1]), (10_000.0, 0.0, 1_000.0));
    assert!(out.failed_at.is_none());
    // Exactly secured: no call, and the cash stays.
    let out = margin_call(&[day(0), cut], cut, day(0), 460_000.0);
    assert_eq!((out.agreements[0].called[1], out.cash[1]), (0.0, 11_000.0));
}

#[test]
fn capacity_rounds_as_the_agreement_says() {
    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let cal = calendar(1);
    let (c, ids) = bonds(&mut ws, &cal, &[("G", 100.0)]);
    let (rec, _) = UnitRecords::compute(&c, &ws, &cal, MissingPolicy::Fail).unwrap();
    let repo = |rounding| Agreement {
        name: "repo".into(),
        kind: AgreementKind::Secured {
            collateral: vec![(ids[0], Advance::Fixed(0.9))],
            basis: Basis::Market,
            concentration: None,
            limit: f64::INFINITY,
            rate: None,
            maturity: None,
            cure: 0,
            opening: 0.0,
            draw: false,
            sweep: false,
            margin: None,
            rounding,
        },
    };
    let run = |rounding| {
        let cfg = LedgerConfig { initial_cash: 0.0, opening: vec![(ids[0], 333_333.33)], financing: vec![repo(rounding)], ..LedgerConfig::default() };
        hold(&ws, &rec, cfg).agreements[0].capacity[0]
    };
    assert_close!(run(None), 333_333.33 * 0.9);
    assert_eq!(run(Some(Rounding::cents(RoundingMode::Down))), 299_999.99);
}

#[test]
fn a_payable_the_cash_cannot_meet_stays_owed_until_cash_arrives() {
    // 40 of cash, a payable of 100 due at day 1, and a receivable of 80 due at day 2.
    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let cal = calendar(3);
    let (c, ids) = bonds(&mut ws, &cal, &[("G", 100.0)]);
    let (rec, _) = UnitRecords::compute(&c, &ws, &cal, MissingPolicy::Fail).unwrap();
    let due = |amount, at| OpeningDue { currency: EntityId::NONE, amount, settles_at: at };
    let cfg = LedgerConfig {
        initial_cash: 40.0,
        opening: vec![(ids[0], 1.0)],
        opening_dues: vec![due(-100.0, day(1)), due(80.0, day(2))],
        journal: true,
        ..LedgerConfig::default()
    };
    let out = hold(&ws, &rec, cfg);
    assert!(out.max_identity_error <= 1e-12);
    // Day 1: 40 is paid and 60 stays owed: an economic failure, with cash at zero, not below.
    assert_eq!(out.cash, vec![40.0, 0.0, 20.0]);
    assert_eq!(out.overdue, vec![0.0, 60.0, 0.0]);
    assert_eq!(out.failed_at, Some(day(1)));
    // Day 2: the receipt arrives first and pays what is still owed.
    assert!(out.journal.iter().any(|e| e.note.as_deref() == Some("paid in part")));
    assert_eq!((out.final_receivable, out.final_payable), (0.0, 0.0));
}
