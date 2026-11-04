//! The run's own records (spec, change 7), P12: a configuration built in code reruns to the same
//! output, pinned by its manifest; a model upgrade is an attributable delta; a retry after a
//! failed job duplicates no events; definitions declare what they model, and a run that needs
//! more is refused unless it declares the approximation.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use fi_kernel::{
    Calendar, Column, Contract, DecisionCtx, DecisionModel, EntityId, ExecRef, Graph, Input, KnowledgePolicy, Ledger, LedgerConfig, LedgerOutput,
    MissingPolicy, Model, Policy, ResearchFill, Series, Targets, Ts, Unit, UnitRecords, Vintaged, Workspace,
};
use fi_research::{Book, Executor, Manifest, PolicySpec, check_requirements, run_book, support_matrix};
use fi_std::{SuppliedBond, TopN, ZScore};

fn world() -> (Workspace, Calendar, Vec<Arc<dyn Contract>>, Vec<EntityId>) {
    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let cal = Calendar::new((0..60).map(|k| Ts::from_days(19_000 + k)).collect());
    let times = cal.times().to_vec();
    let ids: Vec<EntityId> = ["X", "Y", "Z"].iter().map(|n| ws.entities.intern("bond", n)).collect();
    let n = times.len() * 3;
    let clean = (0..n).map(|i| 100.0 + ((i * 7919) % 13) as f64 * 0.1).collect();
    let signal = (0..n).map(|i| ((i * 104_729) % 17) as f64).collect();
    let cols = vec![Column::new("clean", Unit::Price, clean), Column::new("accrued", Unit::Price, vec![0.0; n]), Column::new("signal", Unit::Number, signal)];
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
    (ws, cal, c, ids)
}

fn fv(versions: usize) -> Model {
    let z = |ddof: f64| Model::cross_section(ZScore { name: "v".into(), input: Input::exact("marks", "signal"), group: None, ddof, min_count: 2 });
    let v = (0..versions).map(|i| (Ts::from_days(19_000 + 20 * i as i64), z(i as f64 * 0.5))).collect();
    Model::cross_section(Vintaged::new("fv", v, false).unwrap())
}

fn book(ids: &[EntityId], cal: &Calendar, n: usize) -> Book {
    Book {
        name: "top".into(),
        policies: vec![PolicySpec {
            name: "top".into(),
            decide: Arc::new(TopN { series: "fv".into(), column: "z".into(), universe: ids.to_vec(), n, gross: 1.0 }),
            schedule: cal.every(5, 0),
            scope: None,
            priority: 0,
            lag: 0,
            time_in_force: None,
        }],
        execution: Executor::Fill(Arc::new(ResearchFill { half_spread_bp: 2.0 })),
        config: LedgerConfig { journal: true, ..LedgerConfig::default() },
    }
}

fn run(versions: usize, n: usize) -> (Manifest, LedgerOutput) {
    let (mut ws, cal, c, ids) = world();
    Graph::new().add(fv(versions)).evaluate(&mut ws, &cal).unwrap();
    let (rec, _) = UnitRecords::compute(&c, &ws, &cal, MissingPolicy::Fail).unwrap();
    let b = book(&ids, &cal, n);
    let out = run_book(&ws, &rec, &b).unwrap();
    let reads: Vec<String> = c.iter().flat_map(|x| x.reads()).collect();
    (Manifest::of(&ws, &rec, &reads, &b, &out, &[]), out)
}

#[test]
fn p12_a_pinned_rerun_reproduces_its_output_and_its_journal() {
    let (a, out_a) = run(2, 1);
    let (b, out_b) = run(2, 1);
    assert_eq!(a, b);
    assert_eq!(a.fingerprint(), b.fingerprint());
    assert!(a.diff(&b).is_empty());
    assert_eq!(out_a.journal, out_b.journal);
    assert!(!out_a.journal.is_empty());
    // The manifest names the model, its versions and its role, and the mode.
    let fv = a.models.iter().find(|m| m.name == "fv").unwrap();
    assert_eq!(fv.vintages.len(), 2);
    assert_eq!(a.mode, "policy backtest");
    let json = a.to_json();
    assert!(json.contains("\"name\":\"fv\"") && json.contains("\"mode\":\"policy backtest\""), "{json}");
}

#[test]
fn p12_a_model_upgrade_is_an_attributable_delta() {
    let (a, _) = run(2, 1);
    let (b, _) = run(3, 1);
    let d = a.diff(&b);
    assert!(d.iter().any(|x| x.starts_with("model fv: fingerprint")), "{d:?}");
    assert!(d.iter().any(|x| x.starts_with("series fv:")), "{d:?}");
    // The new version rescales the z-scores without reordering them: the decisions, and so the
    // output, are unchanged, and the diff says exactly that.
    assert!(d.iter().all(|x| x.starts_with("model fv") || x.starts_with("series fv")), "{d:?}");
    // A changed policy changes the book and the output, and nothing upstream of it.
    let (c, _) = run(2, 2);
    let d = a.diff(&c);
    assert!(d.iter().any(|x| x.starts_with("book:")) && d.iter().any(|x| x.starts_with("policies:")) && d.iter().any(|x| x.starts_with("output:")), "{d:?}");
    assert!(!d.iter().any(|x| x.starts_with("model") || x.starts_with("series")), "{d:?}");
}

/// Fails on its first call, as a job that dies part way.
struct Flaky {
    failed: AtomicBool,
}

impl DecisionModel for Flaky {
    fn name(&self) -> String {
        "flaky".into()
    }
    fn params(&self) -> u64 {
        0
    }
    fn decide(&self, ctx: &DecisionCtx<'_>) -> fi_kernel::Result<Option<Targets>> {
        if ctx.k == 30 && !self.failed.swap(true, Ordering::SeqCst) {
            return Err(fi_kernel::Error::Invalid("the job died".into()));
        }
        Ok(Some(Targets::Weights(vec![(ctx.records.entities()[ctx.k % 3], 0.5)])))
    }
}

#[test]
fn p12_a_retry_after_a_failed_job_duplicates_no_events() {
    let (ws, cal, c, _) = world();
    let (rec, _) = UnitRecords::compute(&c, &ws, &cal, MissingPolicy::Fail).unwrap();
    let flaky = Flaky { failed: AtomicBool::new(false) };
    let schedule = cal.times().to_vec();
    let go = |d: &dyn DecisionModel| {
        let cfg = LedgerConfig { journal: true, ..LedgerConfig::default() };
        Ledger::build(&ws, &rec, vec![Policy::new("p", d, &schedule, 0)], ExecRef::Fill(&ResearchFill::default()), cfg).and_then(Ledger::run)
    };
    assert!(go(&flaky).is_err());
    let retried = go(&flaky).unwrap();
    let clean = go(&Flaky { failed: AtomicBool::new(true) }).unwrap();
    assert_eq!(retried.journal, clean.journal);
    let ids: Vec<u64> = retried.journal.iter().map(|e| e.id).collect();
    assert!(ids.windows(2).all(|w| w[0] < w[1]), "identifiers are a sequence, with no duplicates");
}

#[test]
fn definitions_declare_what_they_model_and_runs_that_need_more_are_refused() {
    let (ws, _, c, _) = world();
    let matrix = support_matrix(&c, &[]);
    assert!(matrix.contains(&("supplied_bond".into(), "pik".into())));
    assert!(matrix.contains(&("supplied_bond".into(), "writedown".into())));
    let req = |f: &str| vec![("supplied_bond".to_owned(), f.to_owned())];
    assert!(check_requirements(&ws, &c, &req("writedown"), &[]).unwrap().is_empty());
    let err = check_requirements(&ws, &c, &req("floating coupon floor"), &[]).unwrap_err().to_string();
    assert!(err.contains("does not model floating coupon floor"), "{err}");
    let approx = check_requirements(&ws, &c, &req("floating coupon floor"), &req("floating coupon floor")).unwrap();
    assert_eq!(approx, vec!["supplied_bond does not model floating coupon floor: approximated, as declared".to_owned()]);
}
