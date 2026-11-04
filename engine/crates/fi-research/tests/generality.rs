//! Generality: surface-level needs built from the core objects alone, with no engine change.
//! Fair-value curves, Merton per issuer, signals across relationships, allocation across sleeves
//! that are portfolios, replaying a recorded book, sizing in any measure, parallel sweeps.

use std::sync::{Arc, Mutex};

use fi_kernel::{
    Column, Composite, Contract, DecisionCtx, DecisionModel, EntityId, FromSeries, Graph, HoldingPolicy, Information, Input, KnowledgePolicy, LedgerConfig,
    MissingPolicy, Model, ModelSpec, Outcomes, ResearchFill, Series, Targets, Ts, Unit, UnitRecords, Vintaged, Workspace, ledger,
};
use fi_research::{Strategy, prepare, run, sweep};
use fi_std::{
    Merton, NelsonSiegel, Replay, RiskParity, RiskParityMethod, TopN, ZScore,
    synth::{self, Config, Universe},
};

fn universe(cfg: &Config) -> (Workspace, Universe) {
    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let u = synth::build(&mut ws, cfg).expect("synthetic universe");
    (ws, u)
}

fn grid_values(ws: &Workspace, series: &str, column: &str) -> Vec<f64> {
    let s = ws.series(series).unwrap();
    s.columns()[s.column(column).unwrap()].values.clone()
}

#[test]
fn fair_value_curve_fits_government_bonds() {
    let (mut ws, u) = universe(&Config { bonds: 10, days: 120, govts: 25, ..Config::default() });
    let mut g = Graph::new();
    g.add(Model::cross_section(NelsonSiegel {
        name: "fv".into(),
        yields: Input::exact("govt", "yield"),
        maturity: Input::exact("govt", "maturity"),
        lambda: 1.8,
        min_points: 5,
    }));
    g.evaluate(&mut ws, &u.calendar).unwrap();
    let r = grid_values(&ws, "fv", "residual");
    let finite: Vec<f64> = r.iter().copied().filter(|x| x.is_finite()).collect();
    assert!(finite.len() > r.len() * 9 / 10);
    let rms = (finite.iter().map(|x| x * x).sum::<f64>() / finite.len() as f64).sqrt();
    assert!(rms < 5e-4, "residual RMS {rms} (the synthetic noise is 2 bp)");
}

#[test]
fn merton_runs_per_issuer_and_reaches_bonds_through_the_relationship() {
    let (mut ws, u) = universe(&Config { bonds: 40, days: 200, issuers: 6, govts: 0, ..Config::default() });
    let mut g = Graph::new();
    g.add(Model::time_series(Merton {
        name: "merton".into(),
        equity: Input::exact("equity", "value"),
        debt: Input::asof("financials", "total_debt"),
        rate: Input::market("rate", "value"),
        horizon: 1.0,
        window: 40,
        annualise: 252.0,
    }));
    g.add(Model::cross_section(ZScore {
        name: "credit_z".into(),
        input: Input::exact("merton", "spread").via("issuer", "issuer"),
        group: None,
        ddof: 0.0,
        min_count: 2,
    }));
    g.evaluate(&mut ws, &u.calendar).unwrap();
    // Debt is filed 45 days after the quarter: Merton cannot start before the first filing is known.
    let pd = grid_values(&ws, "merton", "pd");
    let ni = u.issuers.len();
    let first_known = u.calendar.times().iter().position(|t| t.0 >= u.calendar.times()[0].plus(45 * Ts::DAY).0).unwrap();
    assert!(pd[..first_known.min(40) * ni].iter().all(|x| x.is_nan()));
    let late: Vec<f64> = pd[150 * ni..].iter().copied().filter(|x| x.is_finite()).collect();
    assert!(!late.is_empty() && late.iter().all(|p| *p > 0.0 && *p < 1.0));
    // The issuer-level spread reaches every bond through the relationship.
    let s = ws.series("credit_z").unwrap();
    assert_eq!(s.grid_dims().unwrap().1, u.bonds.len());
    assert!(grid_values(&ws, "credit_z", "z")[150 * u.bonds.len()..].iter().any(|x| x.is_finite()));
}

#[test]
fn risk_parity_across_sleeves_that_are_portfolios() {
    let (mut ws, u) = universe(&Config { bonds: 30, days: 260, issuers: 3, govts: 0, ..Config::default() });
    let mut contracts = u.bond_contracts();
    contracts.push(u.index_contract());
    let (mut rec, _) = UnitRecords::compute(&contracts, &ws, &u.calendar, MissingPolicy::Fail).unwrap();
    let t0 = u.calendar.times()[0];
    let mut sleeves = Vec::new();
    for (name, legs) in [("ig", &u.bonds[..15]), ("hy", &u.bonds[15..])] {
        let e = ws.entities.intern("sleeve", name);
        let w = vec![1.0 / legs.len() as f64; legs.len()];
        let (r, _) = Composite { entity: e, legs: legs.to_vec(), schedule: vec![(t0, w)], roll_cost_bp: 0.0 }.build(&rec).unwrap();
        rec.extend(&r).unwrap();
        sleeves.push(e);
    }
    sleeves.push(u.index);
    for method in [RiskParityMethod::InverseVol, RiskParityMethod::EqualRisk] {
        let s = Strategy {
            name: "rp".into(),
            decide: Arc::new(RiskParity { sleeves: sleeves.clone(), lookback: 60, method, gross: 1.0 }),
            schedule: u.calendar.month_ends(),
            fill: Arc::new(ResearchFill { half_spread_bp: 2.0 }),
            config: LedgerConfig::default(),
        };
        let out = run(&ws, &rec, &s).unwrap();
        assert!(out.trades > 0, "{method:?} never traded");
        assert!(out.max_identity_error <= 1e-9);
        assert!(out.final_holdings.iter().all(|(e, _)| sleeves.contains(e)));
    }
}

/// Each decision's time and weights.
type Log = Vec<(Ts, Vec<(EntityId, f64)>)>;

/// Records the targets of the model it wraps.
struct Recorder {
    inner: TopN,
    log: Mutex<Log>,
}

impl DecisionModel for Recorder {
    fn name(&self) -> String {
        self.inner.name()
    }
    fn params(&self) -> u64 {
        self.inner.params()
    }
    fn decide(&self, ctx: &DecisionCtx<'_>) -> fi_kernel::Result<Option<Targets>> {
        let t = self.inner.decide(ctx)?;
        if let Some(Targets::Weights(w)) = &t {
            self.log.lock().unwrap().push((ctx.t, w.clone()));
        }
        Ok(t)
    }
}

#[test]
fn replay_books_a_recorded_book_identically() {
    let (mut ws, u) = universe(&Config { bonds: 50, days: 200, issuers: 3, govts: 0, ..Config::default() });
    let (rec, _) = UnitRecords::compute(&u.bond_contracts(), &ws, &u.calendar, MissingPolicy::Fail).unwrap();
    let schedule = u.calendar.month_ends();
    let fill = ResearchFill { half_spread_bp: 4.0 };
    let recorder =
        Recorder { inner: TopN { series: "marks".into(), column: "signal".into(), universe: u.bonds.clone(), n: 8, gross: 1.0 }, log: Mutex::new(Vec::new()) };
    let original = ledger::run(&ws, &rec, &recorder, &schedule, &fill, LedgerConfig::default()).unwrap();
    // The recorded book becomes a series: depth 0 of bringing a legacy backtester in.
    let log = recorder.log.into_inner().unwrap();
    let (mut e, mut ev, mut wv) = (Vec::new(), Vec::new(), Vec::new());
    for (t, w) in &log {
        for (x, wi) in w {
            e.push(*x);
            ev.push(*t);
            wv.push(*wi);
        }
    }
    ws.add_series(Series::rows("book", e, ev.clone(), ev, vec![Column::new("weight", Unit::Number, wv)]).unwrap()).unwrap();
    let replay = Replay { series: "book".into(), column: "weight".into(), universe: u.bonds.clone(), quantities: false };
    let replayed = ledger::run(&ws, &rec, &replay, &schedule, &fill, LedgerConfig::default()).unwrap();
    assert_eq!(replayed.version(), original.version());
}

#[test]
fn sweeps_share_records_and_match_single_runs() {
    let (ws, u) = universe(&Config { bonds: 80, days: 200, issuers: 3, govts: 0, ..Config::default() });
    let (rec, _) = UnitRecords::compute(&u.bond_contracts(), &ws, &u.calendar, MissingPolicy::Fail).unwrap();
    let variants: Vec<Strategy> = (0..16)
        .map(|i| Strategy {
            name: format!("v{i}"),
            decide: Arc::new(TopN { series: "marks".into(), column: "signal".into(), universe: u.bonds.clone(), n: 3 + i, gross: 1.0 }),
            schedule: u.calendar.every(5 + i % 3, 0),
            fill: Arc::new(ResearchFill { half_spread_bp: i as f64 }),
            config: LedgerConfig { holding: if i % 2 == 0 { HoldingPolicy::Drift } else { HoldingPolicy::ResetWeights }, ..LedgerConfig::default() },
        })
        .collect();
    let parallel = sweep(&ws, &rec, &variants);
    for (v, p) in variants.iter().zip(parallel) {
        assert_eq!(p.unwrap().version(), run(&ws, &rec, v).unwrap().version(), "{}", v.name);
    }
}

#[test]
fn knowledge_policies_differ_only_where_revisions_exist() {
    let make = |policy| {
        let mut ws = Workspace::new(policy);
        let a = ws.entities.intern("issuer", "A");
        // Debt filed at day 10 as 80, revised at day 40 to 120.
        ws.add_series(
            Series::rows(
                "debt",
                vec![a, a],
                vec![Ts::from_days(0), Ts::from_days(0)],
                vec![Ts::from_days(10), Ts::from_days(40)],
                vec![Column::new("value", Unit::Number, vec![80.0, 120.0])],
            )
            .unwrap(),
        )
        .unwrap();
        ws
    };
    let read = |ws: &Workspace, known: i64| ws.series("debt").unwrap().asof(0, EntityId(0), Ts::from_days(50), Ts::from_days(known)).map(|h| h.value);
    let outcome = |ws: &Workspace| ws.outcome("debt").unwrap().asof(0, EntityId(0), Ts::from_days(50), Ts::MAX).map(|h| h.value);
    let known = make(KnowledgePolicy::AS_KNOWN);
    assert_eq!(read(&known, 5), None, "nothing before the first release");
    assert_eq!(read(&known, 20), Some(80.0), "the original from its release");
    assert_eq!(read(&known, 50), Some(120.0), "the correction from its release");
    assert_eq!(outcome(&known), Some(80.0), "outcomes as booked");
    let first = make(KnowledgePolicy::FIRST_RELEASE);
    assert_eq!(read(&first, 50), Some(80.0), "first release: corrections are never seen");
    let legacy = make(KnowledgePolicy::LEGACY_LATEST);
    assert_eq!(read(&legacy, 20), Some(120.0), "legacy latest dates the revision at the first publication: look-ahead");
    assert_eq!(outcome(&legacy), Some(120.0), "restated");
    let reconcile = make(KnowledgePolicy { information: Information::AsKnown, outcomes: Outcomes::Restated });
    assert_eq!((read(&reconcile, 20), outcome(&reconcile)), (Some(80.0), Some(120.0)), "decisions as known, outcomes restated");
    assert!(KnowledgePolicy::AS_KNOWN.is_point_in_time() && KnowledgePolicy::FIRST_RELEASE.is_point_in_time());
    assert!(!KnowledgePolicy::LEGACY_LATEST.is_point_in_time());
}

/// Model vintages: a model is used only from when it became available, unless the run says it is
/// retrospective; versions of a model each serve from their own date.
#[test]
fn models_are_used_only_once_available() {
    let (mut ws, u) = universe(&Config { bonds: 20, days: 60, issuers: 3, govts: 0, ..Config::default() });
    let from = u.calendar.times()[30];
    let z = |name: &str, ddof: f64| ZScore { name: name.into(), input: Input::exact("marks", "signal"), group: None, ddof, min_count: 2 };
    let wrap = |m: ZScore, spec: fn(ModelSpec) -> ModelSpec| Model::cross_section(Respec(m, spec));
    let mut g = Graph::new();
    g.add(wrap(z("late", 0.0), |s| s.available_from(Ts::from_days(0))));
    g.add(Model::cross_section(
        Vintaged::new("versions", vec![(Ts::MIN, Model::cross_section(z("v1", 0.0))), (from, Model::cross_section(z("v2", 1.0)))], false).unwrap(),
    ));
    g.add(Model::cross_section(Vintaged::new("today", vec![(from, Model::cross_section(z("t1", 0.0)))], false).unwrap()));
    g.add(Model::cross_section(Vintaged::new("today_retro", vec![(from, Model::cross_section(z("t2", 0.0)))], true).unwrap()));
    g.evaluate(&mut ws, &u.calendar).unwrap();
    let mut g1 = Graph::new();
    g1.add(Model::cross_section(z("only_v1", 0.0)));
    g1.add(Model::cross_section(z("only_v2", 1.0)));
    g1.evaluate(&mut ws, &u.calendar).unwrap();
    let n = u.bonds.len();
    let col = |name: &str| ws.series(name).unwrap().columns()[0].values.clone();
    let (today, retro, versions) = (col("today"), col("today_retro"), col("versions"));
    assert!(today[..30 * n].iter().all(|v| v.is_nan()), "no output before the model existed");
    assert!(today[30 * n..].iter().all(|v| v.is_finite()));
    assert!(retro.iter().all(|v| v.is_finite()), "a retrospective run uses it throughout");
    let (v1, v2) = (col("only_v1"), col("only_v2"));
    assert_eq!(&versions[..30 * n], &v1[..30 * n], "the first version before the second was available");
    assert_eq!(&versions[30 * n..], &v2[30 * n..], "the second from its date");
    assert!(col("late").iter().all(|v| v.is_finite()), "available from the epoch: always");
}

/// A model with its declaration adjusted.
struct Respec<M>(M, fn(ModelSpec) -> ModelSpec);

impl<M: fi_kernel::CrossSectionModel> fi_kernel::CrossSectionModel for Respec<M> {
    fn spec(&self) -> ModelSpec {
        (self.1)(self.0.spec())
    }
    fn compute(&self, t: Ts, universe: &[EntityId], x: &[Vec<f64>]) -> fi_kernel::Result<Vec<Vec<f64>>> {
        self.0.compute(t, universe, x)
    }
}

/// Sizing in any measure: targets in DTS become quantities through a per-unit series.
struct InDts {
    at: Ts,
    targets: Vec<(EntityId, f64)>,
}

impl DecisionModel for InDts {
    fn name(&self) -> String {
        "in_dts".into()
    }
    fn params(&self) -> u64 {
        1
    }
    fn decide(&self, ctx: &DecisionCtx<'_>) -> fi_kernel::Result<Option<Targets>> {
        Ok((ctx.t == self.at)
            .then(|| Targets::InMeasure { measure: Arc::new(FromSeries { series: "risk".into(), column: "dts".into() }), targets: self.targets.clone() }))
    }
}

#[test]
fn positions_size_in_any_measure() {
    let (mut ws, u) = universe(&Config { bonds: 12, days: 60, issuers: 2, govts: 0, ..Config::default() });
    let times = u.calendar.times().to_vec();
    let n = u.bonds.len();
    let dts: Vec<f64> = (0..times.len() * n).map(|i| 5.0 + (i % n) as f64).collect();
    ws.add_series(Series::grid("risk", u.bonds.clone(), times.clone(), times.clone(), vec![Column::new("dts", Unit::Number, dts)]).unwrap()).unwrap();
    let (rec, _) = UnitRecords::compute(&u.bond_contracts(), &ws, &u.calendar, MissingPolicy::Fail).unwrap();
    let targets: Vec<(EntityId, f64)> = u.bonds[..5].iter().map(|e| (*e, 1_000_000.0)).collect();
    let d = InDts { at: times[3], targets: targets.clone() };
    let out = ledger::run(&ws, &rec, &d, &[times[3]], &ResearchFill { half_spread_bp: 0.0 }, LedgerConfig::default()).unwrap();
    for (j, (e, q)) in out.final_holdings.iter().enumerate() {
        assert_eq!(*e, targets[j].0);
        assert!((q * (5.0 + j as f64) - 1_000_000.0).abs() < 1e-6, "DTS exposure of {e:?} is {}", q * (5.0 + j as f64));
    }
}

#[test]
fn prepare_evaluates_models_then_steps_contracts() {
    let (mut ws, u) = universe(&Config { bonds: 20, days: 80, issuers: 2, govts: 8, ..Config::default() });
    let mut g = Graph::new();
    g.add(Model::cross_section(ZScore { name: "z".into(), input: Input::exact("marks", "signal"), group: None, ddof: 1.0, min_count: 3 }));
    let contracts: Vec<Arc<dyn Contract>> = u.bond_contracts();
    let p = prepare(&mut ws, &g, &contracts, &u.calendar, MissingPolicy::Fail).unwrap();
    assert_eq!(p.graph.evaluated, vec!["z".to_string()]);
    assert_eq!(p.records.entities().len(), u.bonds.len());
}
