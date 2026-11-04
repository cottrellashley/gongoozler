//! Speed tests against the spec's budgets (docs/spec.md, "Performance budgets") on the synthetic
//! universe. The budgets are for 9,000 bonds × 2,520 days on 16 cores. `--scale` shrinks the bond
//! count, for machines with less memory, and the budgets that grow with it (records, models). A
//! research run's budget stays at 50 ms: its cost is set by holdings and days. A sweep must beat
//! the same variants run one by one by the thread count, up to 4×.
//!
//! ```text
//! cargo run --release -p fi-bench -- [--scale 0.25] [--check]
//! ```
//!
//! `--check` exits non-zero if any budget is missed. Spike S7 is measured too: the model graph's
//! cost against hand-written loops over the same data.
//!
//! **The budgets are the spec's machine's.** A fixed single-threaded loop is timed here and against
//! its time on the machine the budgets were set on (`fi_research::machine`, a 16-thread desktop, 3 Nov
//! 2026): a slower core scales every budget by that ratio, and a machine with fewer than 16 threads
//! scales the parallel ones by the threads it lacks. Both factors are printed. A sweep must beat
//! the same variants run one by one by half the thread count, up to 2×: a runner's threads are
//! often hyperthreads of half as many cores.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use fi_kernel::{Graph, HoldingPolicy, Input, KnowledgePolicy, LedgerConfig, MissingPolicy, Model, ResearchFill, Ts, UnitRecords, Workspace};
use fi_research::{Strategy, run, sweep};
use fi_std::{EwmaVol, TopN, ZScore, synth};
use rayon::prelude::*;

struct Row {
    name: String,
    seconds: f64,
    budget: Option<f64>,
    note: String,
}

fn time<T>(f: impl FnOnce() -> T) -> (T, f64) {
    let t = Instant::now();
    let out = f();
    (out, t.elapsed().as_secs_f64())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let scale: f64 = args.iter().position(|a| a == "--scale").and_then(|i| args.get(i + 1)).and_then(|s| s.parse().ok()).unwrap_or(1.0);
    let check = args.iter().any(|a| a == "--check");
    let bonds = ((9_000.0 * scale).round() as usize).max(10);
    let days = 2_520;
    let threads = rayon::current_num_threads();
    let (serial_budget, parallel_budget) = fi_research::machine::factors(threads);
    println!("fi-bench: {bonds} bonds × {days} days ({} cells), {threads} threads, scale {scale}", bonds * days);
    println!("budgets × {serial_budget:.2} for this machine's cores (calibrated), × {parallel_budget:.2} where parallel");
    let mut rows: Vec<Row> = Vec::new();

    let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
    let cfg = synth::Config { bonds, days, seed: 42, amortising_every: 5, pik_every: 11, gap_rate: 0.0, issuers: 200, govts: 0 };
    let (u, s) = time(|| synth::build(&mut ws, &cfg).expect("synthetic universe"));
    rows.push(Row { name: "generate the synthetic universe".into(), seconds: s, budget: None, note: "not budgeted: test data".into() });

    // Unit records: every bond, every day.
    let contracts = u.bond_contracts();
    let ((records, _), s) = time(|| UnitRecords::compute(&contracts, &ws, &u.calendar, MissingPolicy::Fail).expect("records"));
    rows.push(Row {
        name: "unit records (marks + accrued + flows)".into(),
        seconds: s,
        budget: Some(2.0 * scale * parallel_budget),
        note: format!("{:.0} M records/s", (bonds * days) as f64 / s / 1e6),
    });

    // S7: a cross-sectional model through the graph, against a hand-written loop.
    let mut g = Graph::new();
    g.add(Model::cross_section(ZScore {
        name: "z".into(),
        input: Input::exact("marks", "signal"),
        group: Some(Input::asof("sector", "code")),
        ddof: 0.0,
        min_count: 2,
    }));
    let (_, s_graph) = time(|| g.evaluate(&mut ws, &u.calendar).expect("graph"));
    let marks = ws.series("marks").unwrap().clone();
    let sig = marks.column("signal").unwrap();
    let sector: Vec<f64> = u.bonds.iter().map(|e| ws.series("sector").unwrap().asof(0, *e, Ts::MAX, Ts::MAX).map_or(f64::NAN, |h| h.value)).collect();
    let (z_hand, s_hand) = time(|| hand_zscore(marks.columns()[sig].values.as_slice(), &sector, bonds));
    let z_graph = &ws.series("z").unwrap().columns()[0].values;
    let same = z_graph.iter().zip(&z_hand).all(|(a, b)| a.to_bits() == b.to_bits() || (a - b).abs() < 1e-12);
    rows.push(Row {
        name: "cross-section z-score by sector, graph".into(),
        seconds: s_graph,
        budget: Some(2.0 * scale * parallel_budget),
        note: format!("S7: {:.1}× the hand-written loop; outputs equal: {same}", s_graph / s_hand),
    });
    rows.push(Row { name: "cross-section z-score by sector, hand-written".into(), seconds: s_hand, budget: None, note: "baseline".into() });

    // S7: a time-series model through the graph, against a hand-written loop.
    let mut g2 = Graph::new();
    g2.add(Model::time_series(EwmaVol { name: "vol".into(), input: Input::exact("marks", "clean"), halflife: 20.0, annualise: 252.0, warmup: 5 }));
    let (_, s_graph) = time(|| g2.evaluate(&mut ws, &u.calendar).expect("graph"));
    let clean = marks.column("clean").unwrap();
    let (_, s_hand) = time(|| hand_ewma(marks.columns()[clean].values.as_slice(), bonds, 20.0));
    rows.push(Row {
        name: "time-series EWMA volatility, graph".into(),
        seconds: s_graph,
        budget: Some(2.0 * scale * parallel_budget),
        note: format!("S7: {:.1}× the hand-written loop", s_graph / s_hand),
    });
    rows.push(Row { name: "time-series EWMA volatility, hand-written".into(), seconds: s_hand, budget: None, note: "baseline".into() });

    // One research run on cached records: monthly top-300, no optimiser.
    let strategy = |n: usize, bp: f64, holding: HoldingPolicy| Strategy {
        name: format!("top{n}"),
        decide: Arc::new(TopN { series: "marks".into(), column: "signal".into(), universe: u.bonds.clone(), n, gross: 1.0 }),
        schedule: u.calendar.month_ends(),
        fill: Arc::new(ResearchFill { half_spread_bp: bp }),
        config: LedgerConfig { holding, execution_lag: 1, ..LedgerConfig::default() },
    };
    let n = 300.min(bonds / 3).max(1);
    let one = strategy(n, 5.0, HoldingPolicy::Drift);
    let (out, s) = time(|| run(&ws, &records, &one).expect("run"));
    rows.push(Row {
        name: format!("one research run: monthly top-{n}, {} rebalances", u.calendar.month_ends().len()),
        seconds: s,
        // A run's cost is set by its holdings and calendar, which do not shrink with the scale.
        budget: Some(0.05 * serial_budget),
        note: format!("{} trades, identity error {:.1e}", out.trades, out.max_identity_error),
    });

    // A sweep: 64 variants in parallel on the same records.
    let variants: Vec<Strategy> = (0..64)
        .map(|i| strategy(n / 2 + i * n / 64, f64::from(i as u32 % 8), if i % 2 == 0 { HoldingPolicy::Drift } else { HoldingPolicy::ResetWeights }))
        .collect();
    let (results, s) = time(|| sweep(&ws, &records, &variants));
    let ok = results.iter().filter(|r| r.is_ok()).count();
    let (serial, s_serial) = time(|| variants.iter().map(|v| run(&ws, &records, v)).collect::<Vec<_>>());
    let same = results.iter().zip(&serial).all(|(a, b)| matches!((a, b), (Ok(a), Ok(b)) if a.version() == b.version()));
    // The spec asks for sweeps parallel across cores: at least half the thread count, up to 2×.
    rows.push(Row {
        name: "sweep: 64 variants in parallel".into(),
        seconds: s,
        budget: Some(s_serial / (threads.min(4) as f64 / 2.0).max(1.0)),
        note: format!("{ok}/64 ok, {:.0} runs/s, {:.1}× the serial {s_serial:.3}s, same accounts: {same}", 64.0 / s, s_serial / s),
    });

    println!();
    println!("| test | seconds | budget | status | note |");
    println!("| --- | ---: | ---: | --- | --- |");
    let mut failed = 0;
    for r in &rows {
        let (budget, status) = match r.budget {
            Some(b) if r.seconds <= b => (format!("{b:.3}"), "pass"),
            Some(b) => {
                failed += 1;
                (format!("{b:.3}"), "MISS")
            }
            None => ("—".into(), "—"),
        };
        println!("| {} | {:.4} | {budget} | {status} | {} |", r.name, r.seconds, r.note);
    }
    if check && failed > 0 {
        println!("\n{failed} budget(s) missed");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// The same z-score by group as the graph's model, written directly over the grid.
fn hand_zscore(x: &[f64], group: &[f64], n: usize) -> Vec<f64> {
    let t = x.len() / n;
    let mut out = vec![f64::NAN; x.len()];
    out.par_chunks_mut(n).enumerate().for_each(|(ti, z)| {
        let row = &x[ti * n..(ti + 1) * n];
        let mut stats: std::collections::HashMap<u64, (f64, f64, usize)> = std::collections::HashMap::new();
        for j in 0..n {
            if row[j].is_finite() && group[j].is_finite() {
                let s = stats.entry(group[j].to_bits()).or_default();
                s.0 += row[j];
                s.2 += 1;
            }
        }
        for (k, s) in stats.iter_mut() {
            let mean = s.0 / s.2 as f64;
            let var = (0..n).filter(|&j| row[j].is_finite() && group[j].to_bits() == *k).map(|j| (row[j] - mean).powi(2)).sum::<f64>() / s.2 as f64;
            s.1 = var.sqrt();
            if s.2 >= 2 && s.1 > 0.0 {
                for j in 0..n {
                    if row[j].is_finite() && group[j].to_bits() == *k {
                        z[j] = (row[j] - mean) / s.1;
                    }
                }
            }
        }
    });
    let _ = t;
    out
}

/// EWMA volatility per entity, directly over the time-major grid.
fn hand_ewma(x: &[f64], n: usize, halflife: f64) -> Vec<f64> {
    let t = x.len() / n;
    let lambda = 0.5f64.powf(1.0 / halflife);
    (0..n)
        .into_par_iter()
        .map(|j| {
            let (mut last, mut var, mut count) = (f64::NAN, 0.0, 0usize);
            let mut v = vec![f64::NAN; t];
            for ti in 0..t {
                let p = x[ti * n + j];
                if p.is_finite() && p > 0.0 {
                    if last.is_finite() {
                        let r = (p / last).ln();
                        var = if count == 0 { r * r } else { lambda * var + (1.0 - lambda) * r * r };
                        count += 1;
                    }
                    last = p;
                }
                if count >= 5 {
                    v[ti] = (var * 252.0).sqrt();
                }
            }
            v
        })
        .flatten()
        .collect()
}
