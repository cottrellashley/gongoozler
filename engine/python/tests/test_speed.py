"""Speed from Python, against the spec's budgets (docs/spec.md, "Performance budgets").

The spec's budgets are for 9,000 contracts × 2,520 days on 16 cores. These tests use
``FI_SPEED_BONDS`` bonds (default 1,000) × 2,520 days and scale the size-dependent budgets
linearly. Budgets the spec does not set are marked "ours": they bound what Python adds at the
boundary. Run with ``-s`` to see the table.

Every budget here is scaled to this machine by the calibration, which is approximate on other
machines (kb/measurements.md at the repository's root): ``FI_SPEED_TOLERANCE`` (CI: 1.5) holds them
to that many times their value. On the machine the budgets were set on, it is 1.
"""

from __future__ import annotations

import os
import time

import numpy as np
import polars as pl
import pytest

import fi_engine as fi

BONDS = int(os.environ.get("FI_SPEED_BONDS", "1000"))
DAYS = 2520
SCALE = BONDS / 9000
# The budgets are the spec's machine's: this one's speed against it scales them (fi_research::machine).
SERIAL, PARALLEL = fi._native.machine_factors()
TOLERANCE = max(1.0, float(os.environ.get("FI_SPEED_TOLERANCE", "1")))
ROWS: list[tuple[str, float, float, str]] = []


def timed(f):
    t = time.perf_counter()
    out = f()
    return out, time.perf_counter() - t


def best(f, n: int = 3):
    """The result and the best of `n` timings: noise on a shared machine only ever adds time, so the
    least is the estimate of what the work itself costs."""
    out, times = None, []
    for _ in range(n):
        out, s = timed(f)
        times.append(s)
    return out, min(times)


def check(name: str, seconds: float, budget: float, note: str = "") -> None:
    if budget < seconds <= budget * TOLERANCE:
        note += f"; within the calibration's error ({TOLERANCE}×)"
    ROWS.append((name, seconds, budget, note))
    assert seconds <= budget * TOLERANCE, f"{name}: {seconds:.4f}s over its {budget:.4f}s budget (× {TOLERANCE} allowed)"


@pytest.fixture(scope="module", autouse=True)
def table():
    yield
    held = f", held to {TOLERANCE}×" if TOLERANCE > 1 else ""
    print(f"\n\nfi_engine from Python: {BONDS} bonds × {DAYS} days; budgets × {SERIAL:.2f} single-threaded, × {PARALLEL:.2f} parallel{held}\n")
    print("| test | seconds | budget | note |\n| --- | ---: | ---: | --- |")
    for name, s, b, note in ROWS:
        print(f"| {name} | {s:.4f} | {b:.4f} | {note} |")


@pytest.fixture(scope="module")
def panel():
    rng = np.random.default_rng(0)
    cal = fi.Calendar.weekdays("2015-01-05", "2025-12-31").prefix(DAYS)
    ids = np.array([f"US{j:06d}" for j in range(BONDS)])
    df = pl.DataFrame({
        "isin": np.tile(ids, DAYS),
        "date": np.repeat(np.array(cal.times(), dtype="datetime64[ns]"), BONDS),
        "clean": (100 + np.cumsum(rng.normal(0, 0.1, (DAYS, BONDS)), axis=0)).ravel(),
        "accrued": np.tile((np.arange(DAYS) % 126)[:, None] * 0.02, (1, BONDS)).ravel(),
        "signal": rng.normal(0, 1, DAYS * BONDS),
    })
    return df


@pytest.fixture(scope="module")
def loaded(panel):
    ws = fi.Workspace()
    values = {"clean": "price", "accrued": "price", "signal": "number"}
    _, s = timed(lambda: ws.series("marks", panel, entity="isin", kind="bond", time="date", values=values, grid=True))
    rows = panel.height
    check("register a Polars frame (checked, interned, stored)", s, rows / 2e6 * SERIAL, f"ours: 2 M rows/s; {rows / s / 1e6:.1f} M rows/s")
    contracts = ws.supplied_bonds("marks")
    rec, s = timed(lambda: ws.records(contracts, ws.calendar("marks")))
    check("unit records", s, 2.0 * SCALE * PARALLEL, f"spec: 2 s at 9,000; {BONDS * DAYS / s / 1e6:.0f} M records/s")
    return ws, rec


def top(ctx, n):
    v = ctx.cross_section("marks", "signal")
    ok = np.flatnonzero(np.isfinite(v))
    order = ok[np.lexsort((ok, -v[ok]))][:n]
    names = ctx.contracts
    return {names[i]: 1.0 / len(order) for i in order}


def test_runs_cost_what_they_cost_in_rust(loaded):
    ws, rec = loaded
    n = min(300, BONDS // 3)
    built_in = fi.Strategy(fi.Decision.top_n("marks.signal", n), fill=fi.Fill.research(5.0))
    run, s = best(lambda: ws.run(built_in, rec))
    check(f"one research run, monthly top-{n}", s, 0.05 * SERIAL, f"spec: 50 ms; {run.trades} trades")
    python = fi.Strategy(fi.Decision.python(lambda ctx: top(ctx, n)), fill=fi.Fill.research(5.0))
    again, s = best(lambda: ws.run(python, rec))
    calls = len(rec.calendar.month_ends())
    builtin = ROWS[-1][1]
    # What a Python decision adds, against the built-in one on the same machine: a ratio, so it
    # measures the boundary rather than the hardware.
    check(f"the same run, its decision in Python ({calls} calls)", s, 2.0 * builtin, f"ours: at most twice the built-in run; {s / builtin:.2f}×, identical accounts")
    assert again.version == run.version
    ledger, s = timed(run.ledger)
    check("the accounts back as a Polars frame", s, 0.01, f"ours; {ledger.height} rows")


def test_sweeps_run_in_parallel(loaded):
    ws, rec = loaded
    variants = [fi.Strategy(fi.Decision.top_n("marks.signal", 20 + 5 * i), fill=fi.Fill.research(i % 4)) for i in range(32)]
    swept, parallel = timed(lambda: ws.sweep(variants, rec))
    single, serial = timed(lambda: [ws.run(v, rec) for v in variants])
    assert [r.version for r in swept] == [r.version for r in single]
    # Parallel across cores, held as half the ideal speedup up to four threads: a runner's four
    # threads are two cores, hyperthreaded (GitHub's gave 2.4× where four were asked).
    threads = os.cpu_count() or 1
    want = max(1.0, min(4, threads) / 2)
    check("sweep: 32 variants in parallel", parallel, serial / want, f"spec: parallel across cores; {serial / parallel:.1f}× the serial {serial:.3f}s")


def test_python_models_cost_little_per_call(loaded):
    ws, _ = loaded
    cal = ws.calendar("marks")

    @fi.cross_section("zp", ["marks.signal"], ["z"])
    def zp(t, entities, x):
        s = x[0]
        return (s - s.mean()) / s.std()

    _, s = timed(lambda: ws.evaluate([zp], cal))
    check(f"a Python cross-section model ({len(cal)} calls)", s, len(cal) * 1e-4 * SERIAL, f"ours: 100 µs a call; {s / len(cal) * 1e6:.0f} µs a call")
    _, b = timed(lambda: ws.evaluate([fi.Model.zscore("z", "marks.signal")], cal))
    check("the built-in z-score", b, 2.0 * SCALE * PARALLEL, f"{s / b:.1f}× faster than the Python one")
    z, zpy = ws.frame("z")["z"].to_numpy(), ws.frame("zp")["z"].to_numpy()
    assert np.allclose(z, zpy, atol=1e-12, equal_nan=True)
