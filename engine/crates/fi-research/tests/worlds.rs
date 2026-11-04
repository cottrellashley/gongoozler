//! Worlds (spec, change 6): a world is a workspace; stochastic models write its series with
//! keyed draws; waterfalls run inside each world before any average (F25); one borrower's
//! default reaches every exposure to it, and each deal runs once per world (F23); policies on the
//! same world share its draws.

use std::sync::Arc;

use fi_kernel::{
    BatchModel, Calendar, Column, Contract, CrossSectionModel, EntityId, Graph, Input, Intervention, KnowledgePolicy, MissingPolicy, Mode, Model, ModelRole,
    ModelSpec, Producer, Result, Series, Shape, Ts, Unit, UnitRecords, Workspace, hash::Hash64,
};
use fi_research::{Summary, worlds};
use fi_std::structured::{Deal, Losses, Tranche};
use fi_std::{Defaults, GaussianPaths, LoanTerms, SuppliedBond};

mod common;
use common::*;

/// A world's pool loss: `loss` of 100 written down at t2. A real-world model: its output is a
/// simulated world.
struct PoolLoss {
    loan: EntityId,
    loss: f64,
}

impl BatchModel for PoolLoss {
    fn spec(&self) -> ModelSpec {
        let mut s = ModelSpec::new("pool", Shape::Batch);
        s.writes = vec!["pool_flows".into()];
        s.role = ModelRole::RealWorld;
        let mut h = Hash64::new();
        h.f64(self.loss);
        s.params = h.finish();
        s
    }

    fn evaluate(&self, _ws: &Workspace, _cal: &Calendar) -> Result<Vec<Series>> {
        let cols = ["interest", "principal_face", "writedown_face"]
            .iter()
            .map(|c| Column::new(*c, Unit::Number, vec![if *c == "writedown_face" { self.loss / 100.0 } else { 0.0 }]))
            .collect();
        Ok(vec![Series::rows("pool_flows", vec![self.loan], vec![day(2)], vec![day(2)], cols)?])
    }
}

/// A pricing model of M's expected loss given what is known at each time: before the loss date,
/// the declared risk-neutral half chance of a loss of 20; from it, the realised loss.
struct ExpectedLoss {
    at: Ts,
}

impl CrossSectionModel for ExpectedLoss {
    fn spec(&self) -> ModelSpec {
        let mut s = ModelSpec::new("m_expected_loss", Shape::CrossSection).input(Input::exact("d.collateral", "losses"));
        s.outputs = vec!["value".into()];
        s.role = ModelRole::Pricing;
        s.window = 0;
        s
    }

    fn compute(&self, t: Ts, universe: &[EntityId], x: &[Vec<f64>]) -> Result<Vec<Vec<f64>>> {
        let tranche = |l: f64| (l - 10.0).clamp(0.0, 10.0);
        let value: Vec<f64> = x[0].iter().map(|l| if t < self.at { 0.5 * tranche(20.0) + 0.5 * tranche(0.0) } else { tranche(*l) }).collect();
        debug_assert_eq!(value.len(), universe.len());
        Ok(vec![value])
    }
}

/// A pool of 100 with a loss tranche M attaching at 10 and detaching at 20.
fn f25_base() -> (Workspace, Calendar, EntityId, Deal) {
    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let cal = calendar(3);
    let times = cal.times().to_vec();
    let l = ws.entities.intern("loan", "L");
    let marks = vec![Column::new("clean", Unit::Price, vec![100.0; 3]), Column::new("accrued", Unit::Price, vec![0.0; 3])];
    ws.add_series(Series::grid("loans", vec![l], times.clone(), times, marks).unwrap()).unwrap();
    let bond = Arc::new(SuppliedBond {
        entity: l,
        marks: "loans".into(),
        clean: "clean".into(),
        accrued: "accrued".into(),
        flows: Some("pool_flows".into()),
        factor: None,
        reconcile: true,
    }) as Arc<dyn Contract>;
    let tranches =
        [("S", 80.0), ("M", 10.0), ("E", 10.0)].map(|(n, f)| Tranche::debt(n, ws.entities.intern("tranche", n), f, fi_std::structured::Coupon::None)).to_vec();
    let mut d = deal(&mut ws, "d", vec![bond], vec![(l, 100.0)], tranches, vec![day(1), day(2)]);
    d.losses = Losses::WriteDown { order: vec![2, 1, 0], write_up: false };
    (ws, cal, l, d)
}

#[test]
fn f25_the_waterfall_runs_in_each_world_before_the_average() {
    let (ws, cal, l, d) = f25_base();
    let m = ws.entities.get("tranche", "M").unwrap();
    let losses = [0.0, 20.0];
    let per_world = |_: u64, ws: &Workspace, _: &fi_kernel::GraphReport| -> Result<f64> { Ok(total(ws, "d.flows", "writedown_face", m) * 10.0) };
    let graph = |loss: f64| {
        let mut g = Graph::new();
        g.add(Model::batch(PoolLoss { loan: l, loss })).add(Model::batch(d.clone()));
        g
    };
    let m_loss = worlds(&ws, 2, &cal, |w| graph(losses[w as usize]), per_world).unwrap();
    assert_close!(m_loss[0], 0.0);
    assert_close!(m_loss[1], 10.0);
    assert_close!(Summary::of(&m_loss).mean, 5.0);
    // The waterfall on the mean pool loss (10) gives 0: not equivalent, and not what a world
    // average is.
    let at_mean = worlds(&ws, 1, &cal, |_| graph(10.0), per_world).unwrap();
    assert_close!(at_mean[0], 0.0);
}

#[test]
fn f25_a_future_mark_conditions_on_what_is_known_then_and_roles_stay_distinct() {
    let (ws, cal, l, d) = f25_base();
    let losses = [0.0, 20.0];
    let deal_entity = d.entity;
    let marks = worlds(
        &ws,
        2,
        &cal,
        |w| {
            let mut g = Graph::new();
            g.add(Model::batch(PoolLoss { loan: l, loss: losses[w as usize] }))
                .add(Model::batch(d.clone()))
                .add(Model::cross_section(ExpectedLoss { at: day(2) }));
            g
        },
        |_, ws, report| {
            assert_eq!(report.mode(), Mode::Simulation);
            let p = ws.series("m_expected_loss").unwrap().provenance().clone();
            assert_eq!(p.producer, Producer::Model { name: "m_expected_loss".into(), role: ModelRole::Pricing });
            assert_eq!(p.intervention, Some(Intervention::Simulation("pool".into())));
            Ok((at(ws, "m_expected_loss", "value", deal_entity, day(1)), at(ws, "m_expected_loss", "value", deal_entity, day(2))))
        },
    )
    .unwrap();
    // At t1 the worlds have not diverged in anything known: the mark is the same, 5.
    assert_eq!(marks[0].0, 5.0);
    assert_eq!(marks[1].0, 5.0);
    assert_close!(marks[0].1, 0.0);
    assert_close!(marks[1].1, 10.0);
}

/// Loans of three borrowers in two CLOs, and a bond of the first borrower with its own recovery.
fn f23_terms(ws: &mut Workspace) -> (Vec<LoanTerms>, [EntityId; 5]) {
    let [x, y, z] = ["X", "Y", "Z"].map(|n| ws.entities.intern("borrower", n));
    let [lx, ly, lz, lx2, bx] = ["LX", "LY", "LZ", "LX2", "BX"].map(|n| ws.entities.intern("loan", n));
    let terms = |entity, borrower, recovery| LoanTerms { entity, borrower, coupon: 0.06, maturity: day(8), hazard: 0.15, recovery };
    (vec![terms(lx, x, 0.4), terms(ly, y, 0.4), terms(lz, z, 0.4), terms(lx2, x, 0.4), terms(bx, x, 0.6)], [lx, ly, lz, lx2, bx])
}

#[test]
fn f23_one_borrowers_default_reaches_every_exposure_and_each_deal_runs_once_per_world() {
    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let cal = calendar(9);
    let times = cal.times().to_vec();
    let (terms, [lx, ly, lz, lx2, bx]) = f23_terms(&mut ws);
    let ids = [lx, ly, lz, lx2, bx];
    let marks = vec![Column::new("clean", Unit::Price, vec![100.0; 9 * 5]), Column::new("accrued", Unit::Price, vec![0.0; 9 * 5])];
    ws.add_series(Series::grid("loans", ids.to_vec(), times.clone(), times, marks).unwrap()).unwrap();
    let bond = |e| {
        Arc::new(SuppliedBond {
            entity: e,
            marks: "loans".into(),
            clean: "clean".into(),
            accrued: "accrued".into(),
            flows: Some("flows".into()),
            factor: None,
            reconcile: true,
        }) as Arc<dyn Contract>
    };
    let clo = |ws: &mut Workspace, name: &str, pool: &[EntityId]| {
        let a =
            Tranche::debt("A", ws.entities.intern("tranche", &format!("{name}.A")), 60.0 * pool.len() as f64 / 3.0, fi_std::structured::Coupon::Fixed(0.03));
        let e = Tranche::equity("E", ws.entities.intern("tranche", &format!("{name}.E")), 40.0 * pool.len() as f64 / 3.0);
        let mut d =
            deal(ws, name, pool.iter().map(|e| bond(*e)).collect(), pool.iter().map(|e| (*e, 100.0 / 3.0)).collect(), vec![a, e], (1..9).map(day).collect());
        d.interest = vec![interest(&[0]), step(fi_std::structured::Action::Residual(1))];
        d.principal = vec![sequential(&[0])];
        d
    };
    let clo1 = clo(&mut ws, "clo1", &[lx, ly, lz]);
    let clo2 = clo(&mut ws, "clo2", &[lx2, lz]);
    let defaults = |w: u64| Defaults { name: "flows".into(), loans: terms.clone(), payments: (1..9).map(day).collect(), correlation: 0.3, seed: 11, world: w };
    let out = worlds(
        &ws,
        200,
        &cal,
        |w| {
            let mut g = Graph::new();
            g.add(Model::batch(defaults(w))).add(Model::batch(clo1.clone())).add(Model::batch(clo2.clone()));
            g
        },
        |_, ws, report| {
            // Each deal's collateral and waterfall ran once in this world.
            assert_eq!(report.evaluated.iter().filter(|n| n.starts_with("clo")).count(), 2);
            let when = |e: EntityId| {
                let s = ws.series("flows").unwrap();
                let c = s.column("defaulted").unwrap();
                s.rows_between(e, Ts::MIN, Ts::MAX, Ts::MAX).into_iter().find(|&r| s.value(c, r) == 1.0).map(|r| match s.layout() {
                    fi_kernel::Layout::Rows { event, .. } => event[r],
                    fi_kernel::Layout::Grid { .. } => unreachable!(),
                })
            };
            // X defaults once, and every exposure to X sees it then; recoveries differ by facility.
            assert_eq!(when(lx), when(lx2));
            assert_eq!(when(lx), when(bx));
            if let Some(t) = when(bx) {
                assert_close!(flow(ws, "flows", "principal_face", bx, t), 0.6);
                assert_close!(flow(ws, "flows", "principal_face", lx, t), 0.4);
            }
            // The two portfolios hold tranches and the bond on one set of records.
            let held =
                [ws.entities.get("tranche", "clo1.A").unwrap(), ws.entities.get("tranche", "clo1.E").unwrap(), ws.entities.get("tranche", "clo2.A").unwrap()];
            let tranche = |e| {
                Arc::new(SuppliedBond {
                    entity: e,
                    marks: format!("{}.marks", ws.entities.name(e).split('.').next().unwrap()),
                    clean: "clean".into(),
                    accrued: "accrued".into(),
                    flows: Some(format!("{}.flows", ws.entities.name(e).split('.').next().unwrap())),
                    factor: Some("factor".into()),
                    reconcile: true,
                }) as Arc<dyn Contract>
            };
            let mut contracts: Vec<Arc<dyn Contract>> = held.iter().map(|e| tranche(*e)).collect();
            contracts.push(bond(bx));
            UnitRecords::compute(&contracts, ws, &calendar(9), MissingPolicy::Fail)?;
            Ok((when(lx).is_some(), when(ly).is_some()))
        },
    )
    .unwrap();
    let px = out.iter().filter(|(x, _)| *x).count();
    let both = out.iter().filter(|(x, y)| *x && *y).count();
    // Defaults happen, and are correlated through the common factor.
    assert!(px > 0 && px < 200, "{px}");
    let py = out.iter().filter(|(_, y)| *y).count();
    assert!(both as f64 / 200.0 > (px as f64 / 200.0) * (py as f64 / 200.0), "{both} {px} {py}");
}

#[test]
fn worlds_are_reproducible_whatever_the_threads_and_policies_share_their_draws() {
    let ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let cal = calendar(20);
    let paths = |w: u64| GaussianPaths {
        name: "rate".into(),
        start: vec![(EntityId::NONE, 0.04)],
        mean: 0.04,
        reversion: 0.5,
        drift: 0.0,
        vol: 0.01,
        correlation: 0.0,
        floor: Some(0.0),
        seed: 3,
        world: w,
    };
    let last = |_: u64, ws: &Workspace, _: &fi_kernel::GraphReport| -> Result<f64> { Ok(at(ws, "rate", "value", EntityId::NONE, day(19))) };
    let graph = |w: u64| {
        let mut g = Graph::new();
        g.add(Model::batch(paths(w)));
        g
    };
    let a = worlds(&ws, 64, &cal, graph, last).unwrap();
    let b = worlds(&ws, 64, &cal, graph, last).unwrap();
    assert_eq!(a, b);
    // A world is the same world in any run: world 5 alone is world 5 of the 64.
    let mut alone = ws.clone();
    graph(5).evaluate(&mut alone, &cal).unwrap();
    assert_eq!(at(&alone, "rate", "value", EntityId::NONE, day(19)), a[5]);
    let s = Summary::of(&a);
    assert!(s.sd > 0.0 && s.min >= 0.0);
}

#[test]
fn a_generators_output_is_never_an_observation() {
    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let cal = calendar(3);
    let paths = GaussianPaths {
        name: "rate".into(),
        start: vec![(EntityId::NONE, 0.04)],
        mean: 0.04,
        reversion: 0.5,
        drift: 0.0,
        vol: 0.01,
        correlation: 0.0,
        floor: None,
        seed: 1,
        world: 0,
    };
    Graph::new().add(Model::batch(paths)).evaluate(&mut ws, &cal).unwrap();
    let p = ws.series("rate").unwrap().provenance().clone();
    assert_eq!(p.producer, Producer::Model { name: "rate".into(), role: ModelRole::RealWorld });
    assert_eq!(p.intervention, Some(Intervention::Simulation("rate".into())));
}
