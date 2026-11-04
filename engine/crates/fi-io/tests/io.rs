//! Frames in, checked against their declarations; results out as frames.

use std::sync::Arc;

use fi_io::{Known, SeriesDecl, ledger_frame, register, series_frame};
use fi_kernel::{Contract, Error, KnowledgePolicy, LedgerConfig, MissingPolicy, ResearchFill, Ts, Unit, UnitRecords, Workspace, ledger};
use fi_std::{TopN, synth};
use polars::prelude::*;

fn dates(name: &str, days: &[i32]) -> Column {
    Series::new(name.into(), days.to_vec()).cast(&DataType::Date).unwrap().into_column()
}

fn stamps(name: &str, nanos: &[i64]) -> Column {
    Series::new(name.into(), nanos.to_vec()).cast(&DataType::Datetime(TimeUnit::Nanoseconds, None)).unwrap().into_column()
}

fn frame(ids: &[&str], days: &[i32], published: &[i64], oas: &[Option<f64>]) -> DataFrame {
    DataFrame::new_infer_height(vec![
        Series::new("isin".into(), ids.to_vec()).into_column(),
        dates("date", days),
        stamps("publish_ts", published),
        Series::new("oas_pct".into(), oas.to_vec()).into_column(),
    ])
    .unwrap()
}

#[test]
fn a_declared_frame_reads_point_in_time() {
    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let day = Ts::DAY;
    let df = frame(&["A", "A", "B"], &[19_000, 19_001, 19_000], &[19_000 * day + 1, 19_001 * day + 1, 19_000 * day + 1], &[Some(1.5), None, Some(2.0)]);
    let decl = SeriesDecl::new("analytics").entity("isin", "bond").time("date").known(Known::Column("publish_ts".into())).percent("oas_pct");
    let s = register(&mut ws, &df, &decl).unwrap();
    let a = ws.entities.get("bond", "A").unwrap();
    let hit = s.exact(0, a, Ts::from_days(19_000), Ts::MAX).unwrap();
    assert!((hit.value - 0.015).abs() < 1e-15, "percent became a fraction");
    assert!(s.exact(0, a, Ts::from_days(19_000), Ts::from_days(19_000)).is_none(), "not visible before publication");
    assert!(s.exact(0, a, Ts::from_days(19_001), Ts::MAX).is_none(), "a null is missing");
}

#[test]
fn bad_frames_fail_fast() {
    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let decl = SeriesDecl::new("x").entity("isin", "bond").time("date").value("oas_pct", Unit::Number);
    let dup = frame(&["A", "A"], &[19_000, 19_000], &[0, 0], &[Some(1.0), Some(2.0)]);
    assert!(matches!(register(&mut ws, &dup, &decl), Err(Error::Invalid(m)) if m.contains("duplicate")));
    let nan = frame(&["A"], &[19_000], &[0], &[Some(f64::NAN)]);
    assert!(matches!(register(&mut ws, &nan, &decl), Err(Error::Invalid(m)) if m.contains("NaN")));
    assert!(register(&mut ws, &nan, &SeriesDecl { nan_is_missing: true, ..decl.clone() }).is_ok());
    let missing = SeriesDecl::new("y").entity("cusip", "bond");
    assert!(matches!(register(&mut ws, &nan, &missing), Err(Error::Unknown(_))));
}

#[test]
fn lags_in_rows_and_grids() {
    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let df = frame(&["A", "A", "A", "B", "B", "B"], &[1, 2, 3, 1, 2, 3], &[0; 6], &[Some(1.0), Some(2.0), Some(3.0), Some(4.0), Some(5.0), Some(6.0)]);
    let rows = SeriesDecl::new("lagged").entity("isin", "bond").time("date").known(Known::Rows(1)).value("oas_pct", Unit::Number);
    let s = register(&mut ws, &df, &rows).unwrap();
    let a = ws.entities.get("bond", "A").unwrap();
    assert_eq!(s.exact(0, a, Ts::from_days(1), Ts::from_days(2)).unwrap().value, 1.0, "known one row later");
    assert!(s.exact(0, a, Ts::from_days(1), Ts::from_days(1)).is_none());
    assert!(s.exact(0, a, Ts::from_days(3), Ts::from_days(1_000)).is_none(), "the last row is never known");
    let g = register(&mut ws, &df, &SeriesDecl::new("grid").entity("isin", "bond").time("date").value("oas_pct", Unit::Number).grid()).unwrap();
    assert_eq!(g.grid_dims(), Some((3, 2)));
    assert_eq!(g.grid_row(0, 1), &[2.0, 5.0]);
}

#[test]
fn results_come_back_as_frames() {
    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let u = synth::build(&mut ws, &synth::Config { bonds: 20, days: 60, issuers: 2, govts: 0, ..synth::Config::default() }).unwrap();
    let contracts: Vec<Arc<dyn Contract>> = u.bond_contracts();
    let (rec, _) = UnitRecords::compute(&contracts, &ws, &u.calendar, MissingPolicy::Fail).unwrap();
    let top = TopN { series: "marks".into(), column: "signal".into(), universe: u.bonds.clone(), n: 5, gross: 1.0 };
    let out = ledger::run(&ws, &rec, &top, &u.calendar.month_ends(), &ResearchFill { half_spread_bp: 2.0 }, LedgerConfig::default()).unwrap();
    let df = ledger_frame(&out).unwrap();
    assert_eq!(df.height(), u.calendar.len());
    assert_eq!(
        df.get_column_names().iter().map(|n| n.as_str()).collect::<Vec<_>>(),
        [
            "time",
            "nav",
            "cash",
            "income",
            "price",
            "principal",
            "other",
            "financing",
            "costs",
            "currency",
            "capital",
            "unsettled",
            "overdue",
            "nav_per_unit",
            "turnover",
            "holdings"
        ]
    );
    let marks = series_frame(ws.series("marks").unwrap(), &ws.entities).unwrap();
    assert_eq!(marks.height(), u.calendar.len() * u.bonds.len());
    // A frame we produced registers again, unchanged.
    let back = register(
        &mut ws,
        &marks,
        &SeriesDecl::new("marks_again").entity("entity", "bond").time("time").known(Known::Column("known".into())).value("clean", Unit::Price).grid(),
    )
    .unwrap();
    assert_eq!(back.columns()[0].values, ws.series("marks").unwrap().columns()[0].values);
}

#[test]
fn text_relationships_and_categories_are_interned() {
    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let df = DataFrame::new_infer_height(vec![
        Series::new("isin".into(), vec!["B1", "B2", "B3"]).into_column(),
        Series::new("issuer".into(), vec![Some("ACME"), Some("ACME"), None]).into_column(),
        Series::new("sector".into(), vec!["fin", "ind", "fin"]).into_column(),
    ])
    .unwrap();
    let decl = SeriesDecl::new("static").entity("isin", "bond").reference("issuer", "issuer").category("sector");
    let s = register(&mut ws, &df, &decl).unwrap();
    let b = |n: &str| ws.entities.get("bond", n).unwrap();
    let acme = ws.entities.get("issuer", "ACME").unwrap();
    let issuer = |e| s.asof(0, e, Ts::MAX, Ts::MAX).map(|h| h.value);
    assert_eq!(issuer(b("B1")), Some(acme.as_value()));
    assert_eq!(issuer(b("B2")), Some(acme.as_value()));
    assert_eq!(issuer(b("B3")), None, "a null reference is missing");
    let sector = |e| s.asof(1, e, Ts::MAX, Ts::MAX).unwrap().value;
    assert_eq!(sector(b("B1")), sector(b("B3")));
    assert_ne!(sector(b("B1")), sector(b("B2")));
    assert_eq!(ws.entities.name(fi_kernel::EntityId::from_value(sector(b("B2"))).unwrap()), "ind");
    let out = series_frame(&s, &ws.entities).unwrap();
    let names = |c: &str| out.column(c).unwrap().str().unwrap().iter().map(|v| v.map(str::to_owned)).collect::<Vec<_>>();
    assert_eq!(names("issuer"), vec![Some("ACME".to_owned()), Some("ACME".to_owned()), None], "names at the edges");
    assert_eq!(names("sector"), vec![Some("fin".to_owned()), Some("ind".to_owned()), Some("fin".to_owned())]);
}
