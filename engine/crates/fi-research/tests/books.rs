//! Books (spec, changes 1, 2 and 5): currencies, settlement, the cash and short guards, capital
//! flows, the horizon, orders that work over time, policies, constraints, books that share
//! execution, and checkpoints.

use std::sync::{Arc, Mutex};

use fi_exec::{Capacity, PortfolioTrade, RfqFill, RfqProcess};
use fi_kernel::{
    Breach, Calendar, CapitalFlows, CashPolicy, Column, Constraint, ConstraintScope, Contract, DecisionCtx, DecisionModel, EachOrder, EntityId, EntryKind,
    ExecRef, Horizon, InCurrency, KnowledgePolicy, Ledger, LedgerConfig, LedgerOutput, MissingPolicy, OrderStatus, Policy, ResearchFill, Series, Settlement,
    ShortPolicy, Targets, Ts, Unit, UnitRecords, Value, Workspace, ledger,
};
use fi_std::{SuppliedBond, TotalReturn, synth::Rng};
use proptest::prelude::*;

struct World {
    ws: Workspace,
    cal: Calendar,
    a: EntityId,
    b: EntityId,
    c: EntityId,
    usd: EntityId,
    eur: EntityId,
    gbp: EntityId,
}

/// Three levels (A in USD, B in EUR, C in GBP), FX for EUR and GBP in USD, and a 5% rate.
fn world(seed: u64, days: usize, flat_b: bool) -> World {
    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let cal = Calendar::weekdays(Ts::from_days(19_000), Ts::from_days(19_000 + days as i64 * 2)).prefix(days);
    let times = cal.times().to_vec();
    let usd = ws.entities.intern("currency", "USD");
    let eur = ws.entities.intern("currency", "EUR");
    let gbp = ws.entities.intern("currency", "GBP");
    let [a, b, c] = ["A", "B", "C"].map(|n| ws.entities.intern("index", n));
    let mut rng = Rng::new(seed);
    let walk = |rng: &mut Rng, start: f64, vol: f64| -> Vec<f64> {
        let mut x = start;
        (0..days)
            .map(|k| {
                if k > 0 {
                    x *= 1.0 + vol * rng.normal();
                }
                x
            })
            .collect()
    };
    let (la, lb, lc) = (walk(&mut rng, 100.0, 0.01), if flat_b { vec![100.0; days] } else { walk(&mut rng, 100.0, 0.01) }, walk(&mut rng, 100.0, 0.01));
    let lv: Vec<f64> = (0..days).flat_map(|k| [la[k], lb[k], lc[k]]).collect();
    ws.add_series(Series::grid("lv", vec![a, b, c], times.clone(), times.clone(), vec![Column::new("level", Unit::Price, lv)]).unwrap()).unwrap();
    let (xe, xg) = (walk(&mut rng, 1.1, 0.005), walk(&mut rng, 1.25, 0.005));
    let fx: Vec<f64> = (0..days).flat_map(|k| [xe[k], xg[k]]).collect();
    ws.add_series(Series::grid("fx", vec![eur, gbp], times.clone(), times.clone(), vec![Column::new("rate", Unit::Number, fx)]).unwrap()).unwrap();
    let n = times.len();
    ws.add_series(Series::rows("sofr", vec![EntityId::NONE; n], times.clone(), times, vec![Column::new("rate", Unit::Rate, vec![0.05; n])]).unwrap()).unwrap();
    World { ws, cal, a, b, c, usd, eur, gbp }
}

fn level(e: EntityId, currency: Option<EntityId>) -> Arc<dyn Contract> {
    let c: Arc<dyn Contract> = Arc::new(TotalReturn { entity: e, series: "lv".into(), column: "level".into() });
    match currency {
        Some(currency) => Arc::new(InCurrency { contract: c, currency }),
        None => c,
    }
}

fn records(w: &World) -> UnitRecords {
    let c = vec![level(w.a, None), level(w.b, Some(w.eur)), level(w.c, Some(w.gbp))];
    UnitRecords::compute(&c, &w.ws, &w.cal, MissingPolicy::Fail).unwrap().0
}

/// Fixed intentions at fixed times; records what each decision saw.
struct Fixed {
    at: Vec<Ts>,
    targets: Vec<Targets>,
    seen: Mutex<Vec<(usize, usize)>>,
}

fn fixed(at: Vec<Ts>, targets: Vec<Targets>) -> Fixed {
    Fixed { at, targets, seen: Mutex::new(Vec::new()) }
}

impl DecisionModel for Fixed {
    fn name(&self) -> String {
        "fixed".into()
    }
    fn params(&self) -> u64 {
        0
    }
    fn decide(&self, ctx: &DecisionCtx<'_>) -> fi_kernel::Result<Option<Targets>> {
        self.seen.lock().unwrap().push((ctx.holdings.len(), ctx.orders.len()));
        Ok(self.at.iter().position(|t| *t == ctx.t).map(|i| self.targets[i].clone()))
    }
}

fn in_usd(w: &World) -> LedgerConfig {
    LedgerConfig { base_currency: w.usd, fx: Some(("fx".into(), "rate".into())), ..LedgerConfig::default() }
}

fn terms(out: &LedgerOutput) -> f64 {
    (0..out.nav.len())
        .map(|k| out.income[k] + out.price[k] + out.principal[k] + out.other[k] - out.financing[k] + out.currency[k] - out.costs[k] + out.capital[k])
        .sum()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    /// I1 across currencies: NAV in the base currency moves by its terms, the currency term
    /// included, through settlement lags, execution lags and costs; cash never goes below zero.
    #[test]
    fn i1_identity_holds_across_currencies(seed in any::<u64>(), w0 in 0.0f64..0.5, w1 in 0.0f64..0.5, lag in 0usize..3, settle in 0usize..3, bp in 0.0f64..10.0) {
        let w = world(seed, 120, false);
        let rec = records(&w);
        let at = w.cal.month_ends();
        let t = Targets::Weights(vec![(w.a, w0), (w.b, w1), (w.c, (1.0 - w0 - w1).max(0.0))]);
        let d = fixed(at.clone(), vec![t; at.len()]);
        let cfg = LedgerConfig { execution_lag: lag, settlement: Settlement { lag: settle, by_contract: vec![] }, ..in_usd(&w) };
        let out = ledger::run(&w.ws, &rec, &d, &at, &ResearchFill { half_spread_bp: bp }, cfg).unwrap();
        prop_assert!(out.max_identity_error <= 1e-9);
        prop_assert!((out.nav.last().unwrap() - 1e8 - terms(&out)).abs() <= 1e-6 * 1e8);
        let low = out.cash.iter().copied().fold(f64::INFINITY, f64::min);
        prop_assert!(low >= -1e-6, "cash never below zero: {low} at {:?}", out.cash.iter().position(|c| *c == low));
        prop_assert!(out.currency.iter().any(|x| *x != 0.0), "foreign holdings translate");
    }

    /// The cash guard: a book never spends cash it does not have, however much its decisions ask
    /// for; without the guard, the same decisions overdraw.
    #[test]
    fn the_cash_guard_never_lets_cash_fall_below_zero(seed in any::<u64>(), gross in 0.6f64..1.6, bp in 0.0f64..20.0) {
        let w = world(seed, 60, false);
        let rec = records(&w);
        let at = w.cal.month_ends();
        let t = Targets::Weights(vec![(w.a, gross / 2.0), (w.b, gross / 2.0)]);
        let d = fixed(at.clone(), vec![t; at.len()]);
        let fill = ResearchFill { half_spread_bp: bp };
        let guarded = ledger::run(&w.ws, &rec, &d, &at, &fill, in_usd(&w)).unwrap();
        prop_assert!(guarded.cash.iter().all(|c| *c >= -1e-6 * 1e8));
        if gross * (1.0 + bp * 1e-4) > 1.0 + 1e-6 {
            prop_assert!(guarded.cash_limited > 0, "a purchase beyond the cash is cut, and counted");
        }
        let free = ledger::run(&w.ws, &rec, &d, &at, &fill, LedgerConfig { cash: CashPolicy::Unlimited, ..in_usd(&w) }).unwrap();
        if gross > 1.01 {
            prop_assert!(free.cash.iter().any(|c| *c < 0.0), "unlimited borrowing overdraws");
        }
    }
}

#[test]
fn a_foreign_position_with_flat_local_prices_earns_only_currency() {
    let w = world(3, 60, true);
    let rec = records(&w);
    let t0 = w.cal.times()[0];
    let d = fixed(vec![t0], vec![Targets::Quantities(vec![(w.b, 100_000.0)])]);
    let out = ledger::run(&w.ws, &rec, &d, &[t0], &ResearchFill { half_spread_bp: 0.0 }, in_usd(&w)).unwrap();
    let price: f64 = out.price.iter().sum();
    let currency: f64 = out.currency.iter().sum();
    assert!(price.abs() < 1e-9);
    let fx = w.ws.series("fx").unwrap();
    let x = |k: usize| fx.exact(0, w.eur, w.cal.times()[k], Ts::MAX).unwrap().value;
    let expected = 100_000.0 * 100.0 * (x(59) - x(0));
    assert!((currency - expected).abs() < 1e-6 * expected.abs().max(1.0), "{currency} vs {expected}");
    assert!((out.nav[59] - out.nav[0] - currency).abs() < 1e-6);
}

#[test]
fn several_currencies_without_fx_are_refused() {
    let w = world(1, 20, false);
    let rec = records(&w);
    let d = fixed(vec![], vec![]);
    let r = ledger::run(&w.ws, &rec, &d, &[], &ResearchFill { half_spread_bp: 0.0 }, LedgerConfig::default());
    assert!(matches!(r, Err(fi_kernel::Error::Invalid(m)) if m.contains("FX")));
}

#[test]
fn settlement_moves_cash_not_nav() {
    let w = world(5, 60, false);
    let rec = records(&w);
    let times = w.cal.times().to_vec();
    let d = fixed(vec![times[1], times[30]], vec![Targets::Quantities(vec![(w.a, 1000.0)]), Targets::Quantities(vec![(w.a, 0.0)])]);
    let fill = ResearchFill { half_spread_bp: 0.0 };
    let t0 = ledger::run(&w.ws, &rec, &d, &[times[1], times[30]], &fill, in_usd(&w)).unwrap();
    let t2 = ledger::run(&w.ws, &rec, &d, &[times[1], times[30]], &fill, LedgerConfig { settlement: Settlement { lag: 2, by_contract: vec![] }, ..in_usd(&w) })
        .unwrap();
    for k in 0..60 {
        assert!((t0.nav[k] - t2.nav[k]).abs() < 1e-6, "k={k}");
    }
    assert!(t2.unsettled[1] < 0.0 && t2.unsettled[2] < 0.0 && t2.unsettled[3] == 0.0, "the purchase is a payable until it settles");
    assert!((t2.cash[1] - 1e8).abs() < 1e-6 && (t2.cash[3] - t0.cash[3]).abs() < 1e-6, "cash moves at settlement");
    assert!(t2.unsettled[30] > 0.0, "the sale is a receivable until it settles");
}

#[test]
fn r1_a_coupon_on_an_unsettled_purchase_reaches_its_owner() {
    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let cal = Calendar::weekdays(Ts::from_days(19_000), Ts::from_days(19_030)).prefix(15);
    let times = cal.times().to_vec();
    let bond = ws.entities.intern("bond", "B");
    let n = times.len();
    let accrued: Vec<f64> = (0..n).map(|k| if k < 5 { 0.5 * k as f64 } else { 0.5 * (k - 5) as f64 }).collect();
    ws.add_series(
        Series::grid(
            "marks",
            vec![bond],
            times.clone(),
            times.clone(),
            vec![Column::new("clean", Unit::Price, vec![100.0; n]), Column::new("accrued", Unit::Price, accrued)],
        )
        .unwrap(),
    )
    .unwrap();
    ws.add_series(Series::rows("flows", vec![bond], vec![times[5]], vec![times[5]], vec![Column::new("interest", Unit::Number, vec![0.025])]).unwrap())
        .unwrap();
    let c: Vec<Arc<dyn Contract>> = vec![Arc::new(SuppliedBond {
        entity: bond,
        marks: "marks".into(),
        clean: "clean".into(),
        accrued: "accrued".into(),
        flows: Some("flows".into()),
        factor: None,
        reconcile: true,
    })];
    let rec = UnitRecords::compute(&c, &ws, &cal, MissingPolicy::Fail).unwrap().0;
    let d = fixed(vec![times[4]], vec![Targets::Quantities(vec![(bond, 1e6)])]);
    let run = |lag: usize| {
        ledger::run(
            &ws,
            &rec,
            &d,
            &[times[4]],
            &ResearchFill { half_spread_bp: 0.0 },
            LedgerConfig { settlement: Settlement { lag, by_contract: vec![] }, ..LedgerConfig::default() },
        )
        .unwrap()
    };
    let (now, later) = (run(0), run(3));
    let income = |o: &LedgerOutput| o.income.iter().sum::<f64>();
    assert!((income(&now) - income(&later)).abs() < 1e-6, "the economic owner earns the coupon either way");
    assert!((later.unsettled[5] - (-1e6 * 1.02 + 1e6 * 0.025)).abs() < 1e-6, "the seller owes the coupon until settlement");
    assert!((later.cash[7] - now.cash[7]).abs() < 1e-6, "after settlement the cash is the same");
}

#[test]
fn borrowing_is_limited_and_charged() {
    let w = world(7, 40, false);
    let rec = records(&w);
    let times = w.cal.times().to_vec();
    let cfg = LedgerConfig { cash: CashPolicy::Borrow { limit: 1.5e7, rate: Some(("sofr".into(), "rate".into())) }, ..in_usd(&w) };
    let fill = ResearchFill { half_spread_bp: 0.0 };
    // Within the limit: the borrowing is charged at the rate, Act/360.
    let d = fixed(vec![times[0]], vec![Targets::Weights(vec![(w.a, 1.1)])]);
    let out = ledger::run(&w.ws, &rec, &d, &[times[0]], &fill, cfg.clone()).unwrap();
    assert!(out.cash.iter().all(|c| *c >= -1.5e7) && out.cash_limited == 0 && out.failed_at.is_none());
    let days = (times[39].0 - times[0].0) as f64 / Ts::DAY as f64;
    let financing: f64 = out.financing.iter().sum();
    assert!((financing - 1e7 * 0.05 * days / 360.0).abs() < 0.01 * financing, "interest on the borrowing: {financing}");
    // Beyond it: the purchase is cut to the limit; the next day's interest takes the balance past
    // it, and with no cure the book is flagged as failed, never silently overdrawn.
    let d = fixed(vec![times[0]], vec![Targets::Weights(vec![(w.a, 1.2)])]);
    let out = ledger::run(&w.ws, &rec, &d, &[times[0]], &fill, cfg).unwrap();
    assert!(out.cash_limited == 1 && (out.cash[0] + 1.5e7).abs() < 1e-6);
    assert_eq!(out.failed_at, Some(times[1]));
}

#[test]
fn securities_are_shorted_only_against_a_borrow() {
    let w = world(9, 30, false);
    let rec = records(&w);
    let t0 = w.cal.times()[0];
    let d = fixed(vec![t0], vec![Targets::Quantities(vec![(w.a, -1000.0)])]);
    let fill = ResearchFill { half_spread_bp: 0.0 };
    let forbid = ledger::run(&w.ws, &rec, &d, &[t0], &fill, in_usd(&w)).unwrap();
    assert!(forbid.final_holdings.is_empty() && forbid.short_limited == 1);
    let borrow =
        ledger::run(&w.ws, &rec, &d, &[t0], &fill, LedgerConfig { shorts: ShortPolicy::Borrow { fee_bp: 50.0, available: None }, ..in_usd(&w) }).unwrap();
    assert_eq!(borrow.final_holdings, vec![(w.a, -1000.0)]);
    let fees: f64 = borrow.financing.iter().sum();
    assert!(fees > 0.0 && fees < 1000.0 * 150.0 * 50e-4 * 60.0 / 360.0, "a borrow fee accrues: {fees}");
}

#[test]
fn capital_flows_leave_nav_per_unit_unchanged() {
    let mut w = world(11, 40, false);
    let times = w.cal.times().to_vec();
    let flows: Vec<f64> = (0..40)
        .map(|k| match k {
            10 => 1e7,
            20 => -2e7,
            _ => 0.0,
        })
        .collect();
    let n = times.len();
    w.ws.add_series(Series::rows("subs", vec![EntityId::NONE; n], times.clone(), times.clone(), vec![Column::new("amount", Unit::Number, flows)]).unwrap())
        .unwrap();
    let rec = records(&w);
    let d = fixed(vec![times[0]], vec![Targets::Weights(vec![(w.a, 0.5)])]);
    let cfg = LedgerConfig { flows: Some(CapitalFlows::Series { series: "subs".into(), column: "amount".into(), settle: 0 }), ..in_usd(&w) };
    let out = ledger::run(&w.ws, &rec, &d, &[times[0]], &ResearchFill { half_spread_bp: 0.0 }, cfg).unwrap();
    assert_eq!(out.capital.iter().sum::<f64>(), -1e7);
    for k in [10, 20] {
        let r = out.nav_per_unit[k] / out.nav_per_unit[k - 1];
        let without = (out.nav[k] - out.capital[k]) / out.nav[k - 1];
        assert!((r - without).abs() < 1e-12, "the flow buys units at the day's price, not returns");
    }
    assert!(out.max_identity_error <= 1e-9);
}

#[test]
fn r7_a_redemption_due_before_sale_proceeds_settle_is_an_economic_failure() {
    let mut w = world(13, 40, false);
    let times = w.cal.times().to_vec();
    let n = times.len();
    let flows: Vec<f64> = (0..n).map(|k| if k == 20 { -2e7 } else { 0.0 }).collect();
    w.ws.add_series(Series::rows("subs", vec![EntityId::NONE; n], times.clone(), times.clone(), vec![Column::new("amount", Unit::Number, flows)]).unwrap())
        .unwrap();
    let rec = records(&w);
    let d = fixed(vec![times[0], times[20]], vec![Targets::Weights(vec![(w.a, 0.999)]), Targets::Weights(vec![(w.a, 0.75)])]);
    let cfg = LedgerConfig {
        flows: Some(CapitalFlows::Series { series: "subs".into(), column: "amount".into(), settle: 1 }),
        settlement: Settlement { lag: 3, by_contract: vec![] },
        ..in_usd(&w)
    };
    let out = ledger::run(&w.ws, &rec, &d, &[times[0], times[20]], &ResearchFill { half_spread_bp: 0.0 }, cfg.clone()).unwrap();
    assert_eq!(out.failed_at, Some(times[21]), "the redemption falls due before the sale settles");
    assert!(out.shortfall > 1e7);
    assert!(out.returns()[22].is_nan(), "returns are not chained through the failure");
    let failing = LedgerConfig { cash: CashPolicy::Forbid { breach: Breach::Fail }, ..cfg };
    assert!(ledger::run(&w.ws, &rec, &d, &[times[0], times[20]], &ResearchFill { half_spread_bp: 0.0 }, failing).is_err());
}

#[test]
fn the_horizon_liquidates_through_the_execution_model() {
    let w = world(15, 30, false);
    let rec = records(&w);
    let t0 = w.cal.times()[0];
    let d = fixed(vec![t0], vec![Targets::Quantities(vec![(w.a, 1000.0)])]);
    let cfg = LedgerConfig { horizon: Horizon::Liquidate, ..in_usd(&w) };
    let out = ledger::run(&w.ws, &rec, &d, &[t0], &ResearchFill { half_spread_bp: 10.0 }, cfg.clone()).unwrap();
    let value = 1000.0 * rec.value[rec.at(0, 29)];
    let first_cost = out.costs[0];
    assert!((out.liquidation_nav.unwrap() - (out.nav[29] - value * 10e-4)).abs() < 1e-6, "{first_cost}");
    let silent =
        RfqProcess { panel: RfqFill { dealers: 3, response: 0.0, half_spread_bp: 1.0, dispersion_bp: 0.0, max_size: f64::INFINITY, seed: 1 }, latency: 0 };
    let p = vec![Policy::new("p", &d, &[t0], 0)];
    let out = ledger::run_policies(&w.ws, &rec, p, ExecRef::Process(&silent), cfg).unwrap();
    assert!(out.final_holdings.is_empty() && out.trades == 0, "nobody answers");
    let d2 = fixed(vec![t0], vec![Targets::Quantities(vec![(w.a, 1000.0)])]);
    let p2 = vec![Policy::new("p", &d2, &[t0], 0)];
    let filled = RfqProcess { panel: RfqFill { response: 1.0, ..silent.panel.clone() }, latency: 0 };
    let mut l = Ledger::build(&w.ws, &rec, p2, ExecRef::Process(&filled), LedgerConfig { horizon: Horizon::Liquidate, ..in_usd(&w) }).unwrap();
    while l.step().unwrap() {}
    let o = l.finish();
    assert!(o.liquidation_nav.unwrap() < *o.nav.last().unwrap());
}

#[test]
fn p12_f24_a_run_resumed_from_a_checkpoint_matches_the_full_run() {
    let w = world(17, 120, false);
    let rec = records(&w);
    let at = w.cal.month_ends();
    let t = |k: usize| Targets::Weights(vec![(w.a, 0.3 + 0.01 * k as f64), (w.b, 0.3), (w.c, 0.2)]);
    let d = fixed(at.clone(), (0..at.len()).map(t).collect());
    let fill = ResearchFill { half_spread_bp: 3.0 };
    let cfg = LedgerConfig { journal: true, settlement: Settlement { lag: 2, by_contract: vec![] }, ..in_usd(&w) };
    let full = ledger::run(&w.ws, &rec, &d, &at, &fill, cfg.clone()).unwrap();
    let mut first = Ledger::new(&w.ws, &rec, &d, &at, &fill, cfg.clone()).unwrap();
    for _ in 0..61 {
        first.step().unwrap();
    }
    let cp = first.checkpoint();
    assert_eq!(cp.step(), 61);
    let mut resumed = Ledger::new(&w.ws, &rec, &d, &at, &fill, cfg).unwrap();
    resumed.restore(cp);
    while resumed.step().unwrap() {}
    let resumed = resumed.finish();
    while first.step().unwrap() {}
    let first = first.finish();
    assert_eq!(resumed.version(), full.version());
    assert_eq!(first.version(), full.version());
    let ids = |o: &LedgerOutput| o.journal.iter().map(|e| e.id).collect::<Vec<u64>>();
    assert_eq!(ids(&resumed), ids(&full), "the same entries, none duplicated");
    let mut sorted = ids(&full);
    sorted.dedup();
    assert_eq!(sorted.len(), full.journal.len());
}

#[test]
fn r6_p10_orders_work_until_filled_or_expired() {
    let w = world(19, 40, false);
    let rec = records(&w);
    let times = w.cal.times().to_vec();
    let d = fixed(vec![times[1]], vec![Targets::Quantities(vec![(w.a, 1000.0)])]);
    let rfq = RfqProcess { panel: RfqFill { dealers: 2, response: 0.35, half_spread_bp: 2.0, dispersion_bp: 1.0, max_size: 300.0, seed: 4 }, latency: 1 };
    let mut p = Policy::new("p", &d, &times, 0);
    p.time_in_force = Some(6);
    let cfg = LedgerConfig { journal: true, ..in_usd(&w) };
    let out = ledger::run_policies(&w.ws, &rec, vec![p], ExecRef::Process(&rfq), cfg).unwrap();
    let o = &out.order_log;
    assert_eq!(o.len(), 1);
    let order = &o[0];
    assert!(order.attempts >= 1 && order.attempts <= 6);
    let held = out.final_holdings.first().map_or(0.0, |h| h.1);
    assert!((order.filled - held).abs() < 1e-9, "only executed quantities change holdings");
    assert!(order.status == OrderStatus::Filled || order.filled < 1000.0);
    let seen = d.seen.lock().unwrap();
    assert!(seen.iter().skip(2).take(4).any(|(_, working)| *working == 1) || order.status == OrderStatus::Filled, "later decisions see the working order");
    let fills = out.journal.iter().filter(|e| e.kind == EntryKind::Fill).count();
    assert_eq!(fills, out.trades);
}

#[test]
fn policies_keep_to_their_scope_and_priority() {
    let w = world(21, 30, false);
    let rec = records(&w);
    let t0 = w.cal.times()[0];
    let main = fixed(vec![t0], vec![Targets::Weights(vec![(w.a, 0.4)])]);
    let other = fixed(vec![t0], vec![Targets::Quantities(vec![(w.a, 7.0), (w.b, 1000.0)])]);
    let mut p0 = Policy::new("main", &main, &[t0], 0);
    p0.priority = 1;
    p0.scope = Some(vec![w.a]);
    let p1 = Policy::new("other", &other, &[t0], 0);
    let fill = ResearchFill { half_spread_bp: 0.0 };
    let out = ledger::run_policies(&w.ws, &rec, vec![p0, p1], ExecRef::Fill(&fill), LedgerConfig { journal: true, ..in_usd(&w) }).unwrap();
    let q = |e: EntityId| out.final_holdings.iter().find(|h| h.0 == e).map_or(0.0, |h| h.1);
    assert!((q(w.a) - 0.4e8 / rec.value[rec.at(0, 0)]).abs() < 1e-6, "the higher priority keeps A");
    assert_eq!(q(w.b), 1000.0);
    assert!(out.journal.iter().any(|e| e.kind == EntryKind::Note && e.note.as_deref().is_some_and(|n| n.contains("higher-priority"))));
}

#[test]
fn constraints_are_reported_and_mandatory_ones_stay_hard() {
    let soft = Constraint::soft("cap", Arc::new(Value), ConstraintScope::Book, None, Some(0.1), 5.0).mandatory();
    assert!(soft.validate().is_err(), "a mandatory rule cannot become a penalty");
    let mut hard = Constraint::hard("cap", Arc::new(Value), ConstraintScope::Contracts(vec![]), None, Some(0.3)).relative();
    hard.penalty = Some(1.0);
    assert!(hard.validate().is_err(), "a hard rule has no penalty");
    let w = world(23, 30, false);
    let rec = records(&w);
    let t0 = w.cal.times()[0];
    let d = fixed(vec![t0], vec![Targets::Weights(vec![(w.a, 0.5)])]);
    let cap = Constraint::hard("A at most 30%", Arc::new(Value), ConstraintScope::Contracts(vec![w.a]), None, Some(0.3)).relative().mandatory();
    let fill = ResearchFill { half_spread_bp: 0.0 };
    let out = ledger::run(&w.ws, &rec, &d, &[t0], &fill, LedgerConfig { constraints: vec![cap.clone()], ..in_usd(&w) }).unwrap();
    assert!(out.breaches[0].count == 30 && out.breaches[0].first == Some(t0) && out.breaches[0].worst > 0.19);
    assert!(ledger::run(&w.ws, &rec, &d, &[t0], &fill, LedgerConfig { constraints: vec![cap.failing()], ..in_usd(&w) }).is_err());
}

#[test]
fn p11_books_run_together_share_capacity() {
    let w = world(25, 20, false);
    let rec = records(&w);
    let t0 = w.cal.times()[0];
    let v = rec.value[rec.at(0, 0)];
    let d = fixed(vec![t0], vec![Targets::Quantities(vec![(w.a, 10_000.0)])]);
    let shared = Capacity { inner: Arc::new(EachOrder(Arc::new(ResearchFill { half_spread_bp: 0.0 }))), max_value: 10_000.0 * v };
    let book = || Ledger::build(&w.ws, &rec, vec![Policy::new("p", &d, &[t0], 0)], ExecRef::Process(&shared), in_usd(&w)).unwrap();
    let alone = book().run().unwrap();
    assert_eq!(alone.final_holdings, vec![(w.a, 10_000.0)]);
    let together = ledger::run_together(vec![book(), book()], &shared).unwrap();
    for out in &together {
        assert!((out.final_holdings[0].1 - 5_000.0).abs() < 1e-6, "pro rata: {:?}", out.final_holdings);
        assert_eq!(out.unfilled_orders, 1);
    }
}

#[test]
fn p10_a_portfolio_trade_prices_the_basket_as_a_whole() {
    let w = world(27, 20, false);
    let rec = records(&w);
    let t0 = w.cal.times()[0];
    let d = fixed(vec![t0], vec![Targets::Quantities(vec![(w.a, 20_000.0), (w.c, 10_000.0)])]);
    let pt = PortfolioTrade { half_spread_bp: 2.0, impact_bp: 1.0, acceptance: 1.0, seed: 3, latency: 0 };
    let out = ledger::run_policies(&w.ws, &rec, vec![Policy::new("p", &d, &[t0], 0)], ExecRef::Process(&pt), in_usd(&w)).unwrap();
    let (va, vc) = (rec.value[rec.at(0, 0)], rec.value[rec.at(2, 0)]);
    let fx = w.ws.series("fx").unwrap().exact(0, w.gbp, t0, Ts::MAX).unwrap().value;
    let notional = 20_000.0 * va + 10_000.0 * vc;
    let bp = 2.0 + (notional / 1e6).sqrt();
    let expected = (20_000.0 * va + 10_000.0 * vc * fx) * bp * 1e-4;
    assert!((out.costs[0] - expected).abs() < 1e-6 * expected, "{} vs {expected}", out.costs[0]);
}

#[test]
fn a_foreign_coupon_is_translated_once() {
    // A EUR bond pays its coupon on a day the rate moves. The coupon arrives at the new rate, so
    // only the cash held over the interval is translated (a regression: the coupon was
    // translated again, breaking the identity by coupon × ΔFX).
    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let cal = Calendar::new((0..10).map(|k| Ts::from_days(19_000 + k)).collect());
    let times = cal.times().to_vec();
    let usd = ws.entities.intern("currency", "USD");
    let eur = ws.entities.intern("currency", "EUR");
    let b = ws.entities.intern("bond", "EB");
    let marks = vec![Column::new("clean", Unit::Price, vec![100.0; 10]), Column::new("accrued", Unit::Price, vec![0.0; 10])];
    ws.add_series(Series::grid("marks", vec![b], times.clone(), times.clone(), marks).unwrap()).unwrap();
    let flows = vec![Column::new("interest", Unit::Number, vec![0.05])];
    ws.add_series(Series::rows("flows", vec![b], vec![times[5]], vec![times[5]], flows).unwrap()).unwrap();
    let fx: Vec<f64> = (0..10).map(|k| 1.1 + 0.01 * k as f64).collect();
    ws.add_series(Series::grid("fx", vec![eur], times.clone(), times, vec![Column::new("rate", Unit::Number, fx)]).unwrap()).unwrap();
    let bond: Arc<dyn Contract> = Arc::new(SuppliedBond {
        entity: b,
        marks: "marks".into(),
        clean: "clean".into(),
        accrued: "accrued".into(),
        flows: Some("flows".into()),
        factor: None,
        reconcile: true,
    });
    let (rec, _) =
        UnitRecords::compute(&[Arc::new(InCurrency { contract: bond, currency: eur }) as Arc<dyn Contract>], &ws, &cal, MissingPolicy::Fail).unwrap();
    let cfg =
        LedgerConfig { base_currency: usd, fx: Some(("fx".into(), "rate".into())), initial_cash: 0.0, opening: vec![(b, 1_000.0)], ..LedgerConfig::default() };
    let out = Ledger::build(&ws, &rec, vec![], ExecRef::Fill(&ResearchFill::default()), cfg).unwrap().run().unwrap();
    assert!(out.max_identity_error <= 1e-12, "{}", out.max_identity_error);
    // The coupon of 50 EUR is income at that day's rate; the bond and the cash translate.
    assert!((out.income[5] - 50.0 * 1.15).abs() < 1e-9, "{}", out.income[5]);
    assert!((out.nav[9] - (1_000.0 * 1.19 + 50.0 * 1.19)).abs() < 1e-9, "{}", out.nav[9]);
}

#[test]
fn a_sale_stays_legally_owned_until_its_stipulated_settlement() {
    // 1,000 units held; 400 sold at day 1 under a firm bid that stipulates settlement two days
    // later; a coupon of 0.03 a unit at day 2, between the trade and its settlement.
    let ws_for = |days: i64| {
        let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
        let cal = Calendar::new((0..days).map(|k| Ts::from_days(19_000 + k)).collect());
        let times = cal.times().to_vec();
        let b = ws.entities.intern("bond", "B");
        let n = times.len();
        let marks = vec![Column::new("clean", Unit::Price, vec![100.0; n]), Column::new("accrued", Unit::Price, vec![0.0; n])];
        ws.add_series(Series::grid("marks", vec![b], times.clone(), times.clone(), marks).unwrap()).unwrap();
        ws.add_series(Series::rows("flows", vec![b], vec![times[2]], vec![times[2]], vec![Column::new("interest", Unit::Number, vec![0.03])]).unwrap())
            .unwrap();
        let quote = vec![
            Column::new("bid", Unit::Price, vec![0.99]),
            Column::new("size", Unit::Number, vec![400.0]),
            Column::new("settle", Unit::Number, vec![2.0 * 86_400.0]),
        ];
        ws.add_series(Series::rows("quotes", vec![b], vec![times[1]], vec![times[1]], quote).unwrap()).unwrap();
        (ws, cal, b)
    };
    let run = |days: i64| {
        let (ws, cal, b) = ws_for(days);
        let bond: Arc<dyn Contract> = Arc::new(SuppliedBond {
            entity: b,
            marks: "marks".into(),
            clean: "clean".into(),
            accrued: "accrued".into(),
            flows: Some("flows".into()),
            factor: None,
            reconcile: true,
        });
        let (rec, _) = UnitRecords::compute(&[bond], &ws, &cal, MissingPolicy::Fail).unwrap();
        let quotes = fi_exec::QuoteFill::new(&ws, "quotes", Some(("bid", "size")), None, Some("settle")).unwrap();
        let d = fixed(vec![cal.times()[1]], vec![Targets::Orders(vec![(b, -1_000.0)])]);
        let cfg = LedgerConfig { initial_cash: 0.0, opening: vec![(b, 1_000.0)], journal: true, ..LedgerConfig::default() };
        let p = Policy::new("sell", &d, &[cal.times()[1]], 0);
        let out = Ledger::build(&ws, &rec, vec![p], ExecRef::Fill(&quotes), cfg).unwrap().run().unwrap();
        (out, b)
    };
    // Before settlement: economically 600, legally still 1,000. The book received the whole
    // coupon as the legal holder and owes the buyer its share.
    let (out, b) = run(3);
    assert!(out.max_identity_error <= 1e-12);
    assert_eq!(out.final_holdings, vec![(b, 600.0)]);
    assert_eq!(out.final_settled, vec![(b, 1_000.0)]);
    assert!((out.final_receivable - 396.0).abs() < 1e-9, "{}", out.final_receivable);
    assert!((out.final_payable - 12.0).abs() < 1e-9, "{}", out.final_payable);
    assert!((out.income.iter().sum::<f64>() - 600.0 * 0.03).abs() < 1e-9, "the coupon is income only on the economic position");
    assert!(out.journal.iter().any(|e| e.note.as_deref() == Some("owed to the buyer until settlement")));
    // After it: both legs settled once.
    let (out, b) = run(5);
    assert_eq!(out.final_settled, vec![(b, 600.0)]);
    assert_eq!((out.final_receivable, out.final_payable), (0.0, 0.0));
    let cash = out.final_cash.iter().find(|(c, _)| *c == EntityId::NONE).unwrap().1;
    assert!((cash - (1_000.0 * 0.03 - 12.0 + 396.0)).abs() < 1e-9, "{cash}");
}
