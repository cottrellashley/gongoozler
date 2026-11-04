//! The spec's invariants (docs/spec.md), as property tests and targeted scenarios.

use std::sync::Arc;

use fi_kernel::{
    Calendar, Column, Composite, Contract, DecisionCtx, DecisionModel, EntityId, Error, Graph, HoldingPolicy, Input, KnowledgePolicy, Layout, Ledger,
    LedgerConfig, MissingPolicy, Model, ResearchFill, Series, Targets, Ts, Unit, UnitRecords, ValuationState, Workspace, ledger,
};
use fi_research::{Strategy, run};
use fi_std::{
    EwmaVol, Merton, NelsonSiegel, TopN, TotalReturn, ZScore,
    synth::{self, Config, Rng, Universe},
};
use proptest::prelude::*;

const MODELS: [&str; 5] = ["z", "vol", "merton", "fv", "credit_z"];

fn universe(cfg: &Config) -> (Workspace, Universe) {
    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let u = synth::build(&mut ws, cfg).expect("synthetic universe");
    (ws, u)
}

fn models() -> Graph {
    let mut g = Graph::new();
    g.add(Model::cross_section(ZScore {
        name: "z".into(),
        input: Input::exact("marks", "signal"),
        group: Some(Input::asof("sector", "code")),
        ddof: 0.0,
        min_count: 2,
    }));
    g.add(Model::time_series(EwmaVol { name: "vol".into(), input: Input::exact("equity", "value"), halflife: 20.0, annualise: 252.0, warmup: 5 }));
    g.add(Model::time_series(Merton {
        name: "merton".into(),
        equity: Input::exact("equity", "value"),
        debt: Input::asof("financials", "total_debt"),
        rate: Input::market("rate", "value"),
        horizon: 1.0,
        window: 40,
        annualise: 252.0,
    }));
    g.add(Model::cross_section(NelsonSiegel {
        name: "fv".into(),
        yields: Input::exact("govt", "yield"),
        maturity: Input::exact("govt", "maturity"),
        lambda: 1.8,
        min_points: 5,
    }));
    // A bond-level signal from an issuer-level model, through the issuer relationship.
    g.add(Model::cross_section(ZScore {
        name: "credit_z".into(),
        input: Input::exact("merton", "spread").via("issuer", "issuer"),
        group: None,
        ddof: 0.0,
        min_count: 2,
    }));
    g
}

fn top_n(u: &Universe, n: usize, bp: f64, lag: usize, holding: HoldingPolicy) -> Strategy {
    Strategy {
        name: "top".into(),
        decide: Arc::new(TopN { series: "marks".into(), column: "signal".into(), universe: u.bonds.clone(), n, gross: 1.0 }),
        schedule: u.calendar.month_ends(),
        fill: Arc::new(ResearchFill { half_spread_bp: bp }),
        config: LedgerConfig { holding, execution_lag: lag, ..LedgerConfig::default() },
    }
}

/// Fixed targets at fixed times.
struct Fixed {
    at: Vec<Ts>,
    targets: Vec<Vec<(EntityId, f64)>>,
}

impl DecisionModel for Fixed {
    fn name(&self) -> String {
        "fixed".into()
    }
    fn params(&self) -> u64 {
        0
    }
    fn decide(&self, ctx: &DecisionCtx<'_>) -> fi_kernel::Result<Option<Targets>> {
        Ok(self.at.iter().position(|t| *t == ctx.t).map(|i| Targets::Quantities(self.targets[i].clone())))
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]

    /// I1: every unit's terms add up to its change in value plus its cash, and every ledger's
    /// NAV moves by exactly its terms less its costs, through paydowns, PIK, lags and resets.
    #[test]
    fn i1_identity(bonds in 3usize..25, days in 30usize..160, amort in 0usize..5, pik in 0usize..7, seed in any::<u64>(),
                   n in 1usize..10, bp in 0.0f64..20.0, lag in 0usize..3, reset in any::<bool>()) {
        let cfg = Config { bonds, days, seed, amortising_every: amort, pik_every: pik, gap_rate: 0.0, issuers: 3, govts: 0 };
        let (ws, u) = universe(&cfg);
        let (rec, _) = UnitRecords::compute(&u.bond_contracts(), &ws, &u.calendar, MissingPolicy::Fail).unwrap();
        for i in 0..u.bonds.len() {
            for k in 1..u.calendar.len() {
                let a = rec.at(i, k);
                let err = (rec.terms(i, k).total() - (rec.value[a] - rec.value[a - 1] + rec.cash[a])).abs();
                prop_assert!(err <= 1e-12 * rec.value[a].abs().max(1.0), "unit identity off by {err}");
            }
        }
        let holding = if reset { HoldingPolicy::ResetWeights } else { HoldingPolicy::Drift };
        let out = run(&ws, &rec, &top_n(&u, n, bp, lag, holding)).unwrap();
        prop_assert!(out.max_identity_error <= 1e-9);
        let terms: f64 = (0..out.nav.len()).map(|k| out.income[k] + out.price[k] + out.principal[k] + out.other[k] - out.financing[k] - out.costs[k]).sum();
        let initial = LedgerConfig::default().initial_cash;
        prop_assert!((out.nav.last().unwrap() - initial - terms).abs() <= 1e-6 * initial);
    }

    /// I3: holding a composite whole equals holding its legs and trading the rolls, when the roll
    /// cost equals the trading cost.
    #[test]
    fn i3_composite_whole_equals_expanded(seed in any::<u64>(), bp in 0.0f64..10.0, roll_a in 10usize..60, roll_b in 70usize..130, h in 1.0f64..1e6) {
        let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
        let cal = Calendar::weekdays(Ts::from_days(16_800), Ts::from_days(16_800 + 200));
        let times = cal.times().to_vec();
        let tn = times.len();
        let legs: Vec<EntityId> = (0..3).map(|j| ws.entities.intern("future", &format!("F{j}"))).collect();
        let mut rng = Rng::new(seed);
        let mut lv = vec![0.0; tn * 3];
        for j in 0..3 {
            let mut l = 100.0 + 10.0 * rng.uniform();
            for ti in 0..tn {
                if ti > 0 {
                    l *= 1.0 + 0.01 * rng.normal();
                }
                lv[ti * 3 + j] = l;
            }
        }
        ws.add_series(Series::grid("legs", legs.clone(), times.clone(), times.clone(), vec![Column::new("level", Unit::Price, lv)]).unwrap()).unwrap();
        let contracts: Vec<Arc<dyn Contract>> =
            legs.iter().map(|e| Arc::new(TotalReturn { entity: *e, series: "legs".into(), column: "level".into() }) as Arc<dyn Contract>).collect();
        let (mut rec, _) = UnitRecords::compute(&contracts, &ws, &cal, MissingPolicy::Fail).unwrap();
        let comp_e = ws.entities.intern("composite", "C");
        let (ra, rb) = (roll_a.min(tn - 3), roll_b.min(tn - 2).max(roll_a.min(tn - 3) + 1));
        let comp = Composite {
            entity: comp_e,
            legs: legs.clone(),
            schedule: vec![(times[0], vec![1.0, 0.0, 0.0]), (times[ra], vec![0.0, 1.0, 0.0]), (times[rb], vec![0.2, 0.0, 0.8])],
            roll_cost_bp: bp,
        };
        let (crec, qs) = comp.build(&rec).unwrap();
        rec.extend(&crec).unwrap();
        let fill = ResearchFill { half_spread_bp: bp };
        let whole = Fixed { at: vec![times[0]], targets: vec![vec![(comp_e, h)]] };
        let at = vec![times[0], times[ra], times[rb]];
        let expanded = Fixed {
            at: at.clone(),
            targets: at.iter().map(|t| { let k = cal.index(*t).unwrap(); legs.iter().zip(&qs[k]).map(|(e, q)| (*e, h * q)).collect() }).collect(),
        };
        let a = ledger::run(&ws, &rec, &whole, &[times[0]], &fill, LedgerConfig::default()).unwrap();
        let b = ledger::run(&ws, &rec, &expanded, &at, &fill, LedgerConfig::default()).unwrap();
        for k in 0..tn {
            prop_assert!((a.nav[k] - b.nav[k]).abs() <= 1e-9 * a.nav[k].abs().max(1.0), "k={k}: whole {} expanded {}", a.nav[k], b.nav[k]);
        }
    }
}

fn perturb(s: &Series, t_star: Ts, window: i64) -> Series {
    let bump = |x: f64| if x.is_nan() { x } else { x * 1.37 + 11.0 };
    match s.layout() {
        Layout::Grid { entities, times, known, .. } => {
            let n = entities.len();
            let cols = s
                .columns()
                .iter()
                .map(|c| {
                    let mut v = c.values.clone();
                    for (ti, t) in times.iter().enumerate() {
                        if *t > t_star {
                            for x in &mut v[ti * n..(ti + 1) * n] {
                                *x = bump(*x);
                            }
                        }
                    }
                    Column::new(c.name.clone(), c.unit, v)
                })
                .collect();
            Series::grid(s.name(), entities.clone(), times.clone(), known.clone(), cols).unwrap()
        }
        Layout::Rows { entity, event, known, .. } => {
            let cut = t_star.plus(window);
            let cols = s
                .columns()
                .iter()
                .map(|c| {
                    let v = c.values.iter().enumerate().map(|(i, x)| if event[i] > t_star || known[i] > cut { bump(*x) } else { *x }).collect();
                    Column::new(c.name.clone(), c.unit, v)
                })
                .collect();
            Series::rows(s.name(), entity.clone(), event.clone(), known.clone(), cols).unwrap()
        }
    }
}

/// I2 and I9: changing anything that happened after t*, or became known after t* + window, changes
/// no model output at or before t*.
#[test]
fn i9_models_never_see_the_future() {
    let cfg = Config { bonds: 40, days: 260, issuers: 8, govts: 20, ..Config::default() };
    let (mut ws, u) = universe(&cfg);
    let g = models();
    g.evaluate(&mut ws, &u.calendar).unwrap();
    let k_star = 150;
    let t_star = u.calendar.times()[k_star];
    let mut ws2 = Workspace::new(KnowledgePolicy::AS_KNOWN);
    for name in ["marks", "flows", "issuer", "sector", "index", "equity", "financials", "rate", "govt"] {
        ws2.add_series(perturb(ws.series(name).unwrap(), t_star, Ts::DAY)).unwrap();
    }
    g.evaluate(&mut ws2, &u.calendar).unwrap();
    for name in MODELS {
        let (a, b) = (ws.series(name).unwrap(), ws2.series(name).unwrap());
        let n = a.grid_dims().unwrap().1;
        let mut changed_later = false;
        for c in 0..a.columns().len() {
            let (x, y) = (&a.columns()[c].values, &b.columns()[c].values);
            for i in 0..x.len() {
                if i < (k_star + 1) * n {
                    assert_eq!(x[i].to_bits(), y[i].to_bits(), "{name} changed at or before t* (index {i})");
                } else if x[i].to_bits() != y[i].to_bits() {
                    changed_later = true;
                }
            }
        }
        assert!(changed_later, "{name}: the perturbation never reached the model, so the test proves nothing");
    }
}

/// I5: the number of threads and the order of input rows do not change any result.
#[test]
fn i5_threads_and_row_order_do_not_matter() {
    let cfg = Config { bonds: 60, days: 200, issuers: 6, govts: 10, ..Config::default() };
    let run_with = |threads: usize| -> (u64, u64, u64) {
        rayon::ThreadPoolBuilder::new().num_threads(threads).build().unwrap().install(|| {
            let (mut ws, u) = universe(&cfg);
            models().evaluate(&mut ws, &u.calendar).unwrap();
            let (rec, _) = UnitRecords::compute(&u.bond_contracts(), &ws, &u.calendar, MissingPolicy::Fail).unwrap();
            let out = run(&ws, &rec, &top_n(&u, 10, 5.0, 1, HoldingPolicy::Drift)).unwrap();
            (rec.version(), ws.series("merton").unwrap().version(), out.version())
        })
    };
    assert_eq!(run_with(1), run_with(8));
    let rows = |order: &[usize]| {
        Series::rows(
            "x",
            order.iter().map(|&i| EntityId((i % 3) as u32)).collect(),
            order.iter().map(|&i| Ts(i as i64)).collect(),
            order.iter().map(|&i| Ts(i as i64 + 1)).collect(),
            vec![Column::new("v", Unit::Number, order.iter().map(|&i| i as f64).collect())],
        )
        .unwrap()
        .version()
    };
    let fwd: Vec<usize> = (0..50).collect();
    let rev: Vec<usize> = fwd.iter().rev().copied().collect();
    assert_eq!(rows(&fwd), rows(&rev));
}

/// I8: every missing mark is either a failure, or counted under the policy that covered it.
#[test]
fn i8_missing_values_are_counted_never_silent() {
    let cfg = Config { bonds: 30, days: 120, gap_rate: 0.03, ..Config::default() };
    let (ws, u) = universe(&cfg);
    assert!(u.gaps > 0);
    let c = u.bond_contracts();
    assert!(matches!(UnitRecords::compute(&c, &ws, &u.calendar, MissingPolicy::Fail), Err(Error::Missing(_))));
    let (rec, d) = UnitRecords::compute(&c, &ws, &u.calendar, MissingPolicy::Carry { max_age: None }).unwrap();
    assert_eq!(d.snapshot().0 as usize, u.gaps);
    assert_eq!(rec.state.iter().filter(|s| **s == ValuationState::Stale as u8).count(), u.gaps);
    let (_, d) = UnitRecords::compute(&c, &ws, &u.calendar, MissingPolicy::Default { value: 100.0 }).unwrap();
    assert_eq!(d.snapshot().1 as usize, u.gaps);
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// I2: a read at (t, K) sees exactly the observations with event time at or before t (or at
    /// t, for an exact read) known by K: the latest such event wins, and of its vintages the
    /// latest known by K. Missing values (NaN) are never seen.
    #[test]
    fn i2_reads_are_point_in_time(
        rows in prop::collection::vec((0u32..4, 0i64..30, 0i64..40, prop_oneof![1 => Just(f64::NAN), 6 => -5.0f64..5.0]), 0..120),
        t in -2i64..34,
        k in -2i64..44,
    ) {
        let mut rows = rows;
        rows.sort_by_key(|r| (r.0, r.1, r.2));
        rows.dedup_by_key(|r| (r.0, r.1, r.2));
        let s = Series::rows(
            "s",
            rows.iter().map(|r| EntityId(r.0)).collect(),
            rows.iter().map(|r| Ts(r.1)).collect(),
            rows.iter().map(|r| Ts(r.2)).collect(),
            vec![Column::new("v", Unit::Number, rows.iter().map(|r| r.3).collect())],
        )
        .unwrap();
        for e in 0..5u32 {
            let seen = |exact: bool| {
                rows.iter()
                    .filter(|r| r.0 == e && (if exact { r.1 == t } else { r.1 <= t }) && r.2 <= k && !r.3.is_nan())
                    .max_by_key(|r| (r.1, r.2))
                    .map(|r| r.3.to_bits())
            };
            prop_assert_eq!(s.asof(0, EntityId(e), Ts(t), Ts(k)).map(|h| h.value.to_bits()), seen(false));
            prop_assert_eq!(s.exact(0, EntityId(e), Ts(t), Ts(k)).map(|h| h.value.to_bits()), seen(true));
        }
    }
}

/// A model that fails on a chosen date, as a Python model raising would.
struct FailsAt(Ts);

impl fi_kernel::CrossSectionModel for FailsAt {
    fn spec(&self) -> fi_kernel::ModelSpec {
        fi_kernel::ModelSpec::new("fails", fi_kernel::Shape::CrossSection).input(Input::exact("marks", "signal")).output("x")
    }
    fn compute(&self, t: Ts, universe: &[EntityId], _x: &[Vec<f64>]) -> fi_kernel::Result<Vec<Vec<f64>>> {
        if t == self.0 { Err(Error::Invalid("no fit".into())) } else { Ok(vec![vec![0.0; universe.len()]]) }
    }
}

/// I8 and principle 7: failures stop the work and say where, instead of becoming empty results.
#[test]
fn i8_failures_stop_the_run_and_say_where() {
    let (mut ws, u) = universe(&Config { bonds: 20, days: 80, issuers: 3, govts: 0, ..Config::default() });
    let at = u.calendar.times()[40];
    let mut g = Graph::new();
    g.add(Model::cross_section(FailsAt(at)));
    let err = g.evaluate(&mut ws, &u.calendar).unwrap_err();
    assert_eq!(err, Error::Invalid(format!("model fails at {at}: no fit")));
    assert!(!ws.contains("fails"), "a failed model leaves no output behind");
    // A model may not take the name of registered data: every state has one producer.
    let marks = ws.series("marks").unwrap().version();
    let mut g = Graph::new();
    g.add(Model::cross_section(ZScore { name: "marks".into(), input: Input::exact("sector", "code"), group: None, ddof: 0.0, min_count: 2 }));
    assert!(matches!(g.evaluate(&mut ws, &u.calendar), Err(Error::Invalid(m)) if m.contains("registered series")));
    assert_eq!(ws.series("marks").unwrap().version(), marks, "the data is untouched");

    let (rec, _) = UnitRecords::compute(&u.bond_contracts(), &ws, &u.calendar, MissingPolicy::Fail).unwrap();
    // A misspelt series is an error, not a strategy that never trades.
    let mut s = top_n(&u, 5, 0.0, 1, HoldingPolicy::Drift);
    s.decide = Arc::new(TopN { series: "makrs".into(), column: "signal".into(), universe: u.bonds.clone(), n: 5, gross: 1.0 });
    assert!(matches!(run(&ws, &rec, &s), Err(Error::Unknown(m)) if m.contains("makrs")));
    // A value that is not a number fails the identity: a NaN NAV never passes silently.
    let cal = Calendar::new((0..3).map(|d| Ts::from_days(20_000 + d)).collect());
    let one = EntityId(0);
    let nan =
        UnitRecords::from_parts(cal.clone(), vec![one], vec![1.0, f64::NAN, 1.0], vec![0.0; 3], vec![fi_kernel::Terms::default(); 3], vec![0; 3], vec![0; 3])
            .unwrap();
    let buy = Fixed { at: vec![cal.times()[0]], targets: vec![vec![(one, 10.0)]] };
    let r =
        ledger::run(&ws, &nan, &buy, &[cal.times()[0]], &ResearchFill { half_spread_bp: 0.0 }, LedgerConfig { execution_lag: 0, ..LedgerConfig::default() });
    assert!(matches!(r, Err(Error::Invalid(m)) if m.contains("identity")));
    // A decision naming one contract twice is ambiguous: refused.
    let twice = Fixed { at: vec![u.calendar.times()[3]], targets: vec![vec![(u.bonds[0], 1.0), (u.bonds[1], 1.0), (u.bonds[0], 2.0)]] };
    let r = ledger::run(&ws, &rec, &twice, &[u.calendar.times()[3]], &ResearchFill { half_spread_bp: 0.0 }, LedgerConfig::default());
    assert!(matches!(r, Err(Error::Invalid(m)) if m.contains("twice")));
}

/// I10: the same inputs give bit-identical outputs, and a second evaluation is served from the cache.
#[test]
fn i10_outputs_are_reproducible_and_cached() {
    let cfg = Config { bonds: 50, days: 150, issuers: 6, govts: 12, ..Config::default() };
    let (mut ws, u) = universe(&cfg);
    let g = models();
    let first = g.evaluate(&mut ws, &u.calendar).unwrap();
    assert_eq!(first.evaluated.len(), g.len());
    assert!(first.cached.is_empty());
    let v1: Vec<u64> = MODELS.iter().map(|n| ws.series(n).unwrap().version()).collect();
    let second = g.evaluate(&mut ws, &u.calendar).unwrap();
    assert_eq!(second.cached.len(), g.len());
    assert!(second.evaluated.is_empty());
    let (mut ws2, u2) = universe(&cfg);
    g.evaluate(&mut ws2, &u2.calendar).unwrap();
    let v2: Vec<u64> = MODELS.iter().map(|n| ws2.series(n).unwrap().version()).collect();
    assert_eq!(v1, v2);
}

/// Batch and step evaluation agree: evaluating on a prefix of the calendar gives the same values
/// as the full evaluation over that prefix, and stepping the ledger by hand equals running it.
#[test]
fn batch_and_step_agree() {
    let cfg = Config { bonds: 30, days: 120, issuers: 5, govts: 10, ..Config::default() };
    let (mut ws, u) = universe(&cfg);
    let g = models();
    g.evaluate(&mut ws, &u.calendar).unwrap();
    for k in [1, 17, 60, 119] {
        let (mut wk, _) = universe(&cfg);
        g.evaluate(&mut wk, &u.calendar.prefix(k)).unwrap();
        for name in MODELS {
            let (f, p) = (ws.series(name).unwrap(), wk.series(name).unwrap());
            let n = f.grid_dims().unwrap().1;
            for c in 0..f.columns().len() {
                for i in 0..k * n {
                    assert_eq!(f.columns()[c].values[i].to_bits(), p.columns()[c].values[i].to_bits(), "{name} at prefix {k}");
                }
            }
        }
    }
    let (rec, _) = UnitRecords::compute(&u.bond_contracts(), &ws, &u.calendar, MissingPolicy::Fail).unwrap();
    let s = top_n(&u, 5, 3.0, 1, HoldingPolicy::Drift);
    let ran = run(&ws, &rec, &s).unwrap();
    let mut l = Ledger::new(&ws, &rec, s.decide.as_ref(), &s.schedule, s.fill.as_ref(), s.config.clone()).unwrap();
    while l.step().unwrap() {}
    assert_eq!(l.finish().version(), ran.version());
}
