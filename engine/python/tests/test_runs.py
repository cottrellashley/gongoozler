"""Contracts, decisions and runs from Python: exact accounts, built-in and Python decisions alike."""

from __future__ import annotations

import datetime as dt

import numpy as np
import polars as pl
import pytest

import fi_engine as fi
from conftest import level, weekdays


def once(targets, at=None):
    """A Python decision that trades to `targets` at its first call (or at `at`) and then holds."""

    @fi.decision
    def decide(ctx):
        if (at is None and ctx.k == 0) or ctx.t == at:
            return targets
        return None

    return decide


def test_an_index_held_whole_tracks_its_level_exactly():
    df = level(days=60)
    ws = fi.Workspace()
    ws.series("index", df, entity="index", time="date", values={"level": "number"})
    rec = ws.records(ws.total_return("IDX", "index.level"), ws.calendar("index"))
    run = ws.run(fi.Strategy(once({"IDX": 1.0}), schedule="all", execution_lag=0), rec)
    nav = run.ledger()["nav"].to_numpy()
    lv = df["level"].to_numpy()
    assert np.allclose(nav, 1e8 * lv / lv[0], rtol=1e-12, atol=0)
    led = run.ledger()
    assert led["price"].sum() == pytest.approx(nav[-1] - nav[0], rel=1e-12)
    assert run.max_identity_error < 1e-12 and run.trades == 1


def test_a_coupon_is_income_and_only_income():
    """Clean flat at 100; accrued grows 0.01 a day for ten days, then the coupon pays it out."""
    dates = weekdays(dt.date(2024, 1, 1), 15)
    accrued = [0.01 * k if k < 10 else 0.01 * (k - 10) for k in range(15)]
    ws = fi.Workspace()
    ws.series("marks", pl.DataFrame({"isin": ["B"] * 15, "date": dates, "clean": [100.0] * 15, "accrued": accrued}),
              entity="isin", kind="bond", time="date", values={"clean": "price", "accrued": "price"})
    ws.series("flows", pl.DataFrame({"isin": ["B"], "date": [dates[10]], "interest": [0.001]}),
              entity="isin", kind="bond", time="date", values={"interest": "number"})
    rec = ws.records(ws.supplied_bonds("marks", flows="flows"), ws.calendar("marks"))
    run = ws.run(fi.Strategy(once({"B": 1.0}), schedule="all", execution_lag=0), rec)
    led = run.ledger()
    q = 1e8 / 1.0  # bought at (100 + 0) / 100 per unit of face
    assert led["income"][1:11].sum() == pytest.approx(q * 0.001, rel=1e-12)
    assert led["price"].abs().max() < 1e-6 and led["principal"].abs().max() < 1e-6
    assert led["cash"][10] == pytest.approx(q * 0.001, rel=1e-12), "the coupon is cash on its date"
    assert run.final_nav == pytest.approx(1e8 + q * 0.001 + q * 0.0004, rel=1e-12)  # plus four days' accrual


def top(ctx, n):
    """The built-in top-N, written in Python: the best n by signal, ties to the earlier contract."""
    s = ctx.cross_section("marks", "signal")
    ok = np.flatnonzero(np.isfinite(s))
    order = ok[np.lexsort((ok, -s[ok]))][:n]
    names = ctx.contracts
    return {names[i]: 1.0 / len(order) for i in order}


def test_a_python_decision_and_the_built_in_one_book_identical_accounts(synthetic):
    ws, _, rec = synthetic
    builtin = ws.run(fi.Strategy(fi.Decision.top_n("marks.signal", 10), fill=fi.Fill.research(4.0)), rec)
    python = ws.run(fi.Strategy(fi.Decision.python(lambda ctx: top(ctx, 10)), fill=fi.Fill.research(4.0)), rec)
    assert builtin.trades > 0
    assert python.version == builtin.version, "bit-identical accounts"


def test_a_recorded_book_replays_identically(synthetic):
    """Depth 0 of bringing in a legacy backtester: its recorded weights, booked by this ledger."""
    ws, _, rec = synthetic
    log = []

    def recording(ctx):
        w = top(ctx, 8)
        log.extend((name, ctx.t, weight) for name, weight in w.items())
        return w

    original = ws.run(fi.Strategy(fi.Decision.python(recording), holding="reset_weights"), rec)
    book = pl.DataFrame(log, schema=["isin", "t", "weight"], orient="row").with_columns(pl.from_epoch("t", time_unit="ns"))
    ws.series("book", book, entity="isin", kind="bond", time="t", values={"weight": "number"})
    replayed = ws.run(fi.Strategy(fi.Decision.replay("book.weight"), holding="reset_weights"), rec)
    assert replayed.version == original.version


def test_a_sweep_runs_in_parallel_and_matches_single_runs(synthetic):
    ws, _, rec = synthetic
    variants = [fi.Strategy(fi.Decision.top_n("marks.signal", n), fill=fi.Fill.research(bp), holding=h)
                for n in (5, 10, 20) for bp in (0.0, 5.0) for h in ("drift", "reset_weights")]
    swept = ws.sweep(variants, rec)
    assert [r.version for r in swept] == [ws.run(s, rec).version for s in variants]
    costs = {(r.name, r.version): r.ledger()["costs"].sum() for r in swept}
    assert len(costs) == len(variants) and all(c >= 0 for c in costs.values())


def test_python_decisions_sweep_in_parallel_too(synthetic):
    """Python decisions on the sweep's worker threads take the GIL in turn: same accounts."""
    ws, _, rec = synthetic
    variants = [fi.Strategy(fi.Decision.python(lambda ctx, n=n: top(ctx, n)), fill=fi.Fill.research(2.0)) for n in (3, 6, 9, 12, 15, 18)]
    swept = ws.sweep(variants, rec)
    assert [r.version for r in swept] == [ws.run(v, rec).version for v in variants]
    assert [r.version for r in swept] == [ws.run(fi.Strategy(fi.Decision.top_n("marks.signal", n), fill=fi.Fill.research(2.0)), rec).version
                                          for n in (3, 6, 9, 12, 15, 18)]


def test_python_exceptions_come_back_unchanged(synthetic):
    ws, _, rec = synthetic

    def broken(ctx):
        return 1 / 0

    with pytest.raises(ZeroDivisionError):
        ws.run(fi.Strategy(fi.Decision.python(broken)), rec)
    with pytest.raises(KeyError, match="NOPE is not a contract"):
        ws.run(fi.Strategy(fi.Decision.python(lambda ctx: {"NOPE": 1.0})), rec)
    with pytest.raises(fi.UnknownError, match="makrs"):
        ws.run(fi.Strategy(fi.Decision.top_n("makrs.signal", 5)), rec)


def test_a_context_is_valid_only_during_its_decision(synthetic):
    ws, _, rec = synthetic
    kept = []
    ws.run(fi.Strategy(fi.Decision.python(lambda ctx: kept.append(ctx))), rec)
    assert len(kept) == len(rec.calendar.month_ends())
    assert kept[0].nav == 1e8  # plain values stay readable
    with pytest.raises(RuntimeError, match="only during its decision"):
        kept[0].cross_section("marks", "signal")


def test_quantities_and_the_book_a_decision_sees(synthetic):
    ws, _, rec = synthetic
    names = rec.contracts[:3]
    seen = []

    def decide(ctx):
        seen.append(dict(ctx.holdings))
        return fi.quantities({n: 1000.0 * (i + 1) for i, n in enumerate(names)})

    run = ws.run(fi.Strategy(fi.Decision.python(decide)), rec)
    assert seen[0] == {}
    assert seen[2] == {n: pytest.approx(1000.0 * (i + 1)) for i, n in enumerate(names)}
    assert run.final_holdings == {n: pytest.approx(1000.0 * (i + 1)) for i, n in enumerate(names)}


def test_fills_cost_what_they_say():
    df = level(days=30)
    ws = fi.Workspace()
    ws.series("index", df, entity="index", time="date", values={"level": "number"})
    rec = ws.records(ws.total_return("IDX", "index.level"), ws.calendar("index"))
    v0 = df["level"][0]
    q = 1e8 / v0
    # A Nautilus L2 book quoting in points around its mark (at the book's 4 decimals), deep enough.
    m = round(v0, 4)
    book = fi.Fill.book({"IDX": {"bids": [(m - 0.2, 1e9)], "asks": [(m + 0.1, q / 2), (m + 0.3, 1e9)], "mark": m, "per_unit": 1.0}}, size_precision=2)
    # Twice the cash the order needs, so the cash guard does not cut it.
    run = ws.run(fi.Strategy(once(fi.quantities({"IDX": q})), schedule="all", execution_lag=0, fill=book, initial_cash=2e8), rec)
    assert run.ledger()["costs"][0] == pytest.approx(q / 2 * 0.1 + q / 2 * 0.3, rel=1e-6)
    rfq = fi.Fill.rfq(dealers=5, response=1.0, half_spread_bp=3.0, dispersion_bp=0.0)
    run = ws.run(fi.Strategy(once(fi.quantities({"IDX": q})), schedule="all", execution_lag=0, fill=rfq, initial_cash=2e8), rec)
    assert run.ledger()["costs"][0] == pytest.approx(q * v0 * 3e-4, rel=1e-9)
    assert run.max_identity_error < 1e-12


def test_sleeves_are_composites_and_risk_parity_allocates_across_them(synthetic):
    ws, u, rec = synthetic
    first = u["calendar"].times()[0]
    names = rec.contracts
    rec = ws.composite(rec, "ig", names[:10], [(first, [0.1] * 10)])
    rec = ws.composite(rec, "hy", names[10:30], [(first, [0.05] * 20)])
    run = ws.run(fi.Strategy(fi.Decision.risk_parity(["ig", "hy"], 60, method="equal_risk")), rec)
    held = run.final_holdings
    assert set(held) == {"ig", "hy"} and all(q > 0 for q in held.values())
    assert run.max_identity_error < 1e-9


def test_statistics_come_from_nautilus_and_match_their_definition(synthetic):
    ws, _, rec = synthetic
    run = ws.run(fi.Strategy(fi.Decision.top_n("marks.signal", 10)), rec)
    r = run.ledger()["nav"].pct_change().drop_nulls().to_numpy()
    rep = run.report()
    assert rep["sharpe"] == pytest.approx(r.mean() / r.std(ddof=1) * np.sqrt(252), rel=1e-9)
    assert rep["income"] + rep["price"] + rep["principal"] + rep["other"] - rep["financing"] - rep["costs"] == pytest.approx(
        run.final_nav - 1e8, rel=1e-9
    )
    vs_self = run.report(benchmark=run)
    assert vs_self["tracking_error"] == pytest.approx(0.0, abs=1e-12)


def test_risk_parity_in_python_matches_the_built_in_one(synthetic):
    """Inverse-volatility weights across sleeves, from the sleeves' own returns known at the
    decision, in NumPy: the same accounts as the built-in model, to rounding."""
    ws, u, rec = synthetic
    first = u["calendar"].times()[0]
    names = rec.contracts
    rec = ws.composite(rec, "ig", names[:10], [(first, [0.1] * 10)])
    rec = ws.composite(rec, "hy", names[10:30], [(first, [0.05] * 20)])
    sleeves = ["ig", "hy"]

    def inverse_vol(ctx):
        rets = [ctx.returns(s, 60) for s in sleeves]
        m = min(len(r) for r in rets)
        if m < 2:
            return None
        inv = np.array([1 / max(np.std(r[-m:], ddof=1), 1e-12) for r in rets])
        return dict(zip(sleeves, inv / inv.sum()))

    built_in = ws.run(fi.Strategy(fi.Decision.risk_parity(sleeves, 60)), rec)
    python = ws.run(fi.Strategy(fi.Decision.python(inverse_vol)), rec)
    assert built_in.trades == python.trades > 0
    assert np.allclose(python.ledger()["nav"].to_numpy(), built_in.ledger()["nav"].to_numpy(), rtol=1e-12, atol=0)
