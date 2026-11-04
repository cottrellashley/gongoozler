"""The optimiser behind the decision interface (review M6; the brief's §8 and P11): every declared
constraint holds in the book it trades to, the solver's status and binding constraints are in the
journal, and an infeasible mandate is never relaxed silently."""

from __future__ import annotations

import datetime as dt

import numpy as np
import polars as pl
import pytest

import fi_engine as fi
from conftest import weekdays


def world(days: int = 80, n: int = 30, seed: int = 3):
    rng = np.random.default_rng(seed)
    dates = weekdays(dt.date(2025, 1, 6), days)
    names = [f"B{i:02d}" for i in range(n)]
    level = 100 * np.exp(np.cumsum(rng.normal(0, 0.003, (days, n)), axis=0))
    sig = pl.DataFrame(
        {
            "isin": names * days,
            "date": [d for d in dates for _ in names],
            "alpha": rng.normal(0.02, 0.01, days * n),
            "vol": np.tile(rng.uniform(0.03, 0.10, n), days),
            "duration": np.tile(rng.uniform(1.0, 9.0, n), days),
            "sector": np.tile(np.arange(n) % 4, days).astype(float),
        }
    )
    ws = fi.Workspace()
    ws.series("lv", pl.DataFrame({"isin": names * days, "date": [d for d in dates for _ in names], "level": level.ravel()}), entity="isin", kind="bond",
              time="date", values={"level": "number"}, grid=True)
    ws.series("sig", sig, entity="isin", kind="bond", time="date", values={"alpha": "number", "vol": "number", "duration": "number", "sector": "number"}, grid=True)
    contracts = None
    for name in names:
        c = ws.total_return(name, "lv.level")
        contracts = c if contracts is None else contracts + c
    return ws, ws.records(contracts, ws.calendar("lv")), sig


def test_m6_every_constraint_holds_in_the_book_and_the_journal_says_what_bound():
    ws, rec, sig = world()
    decide = fi.Decision.optimise("sig.alpha", risk="sig.vol", risk_aversion=20.0, bounds=(0.0, 0.08), exposures={"sig.duration": (4.0, 4.5)},
                                  groups=("sig.sector", 0.3))
    run = ws.run(fi.Strategy(decide, execution_lag=0, fill=fi.Fill.research(0.0), keep_holdings=True, journal=True), rec)
    held = run.holdings()
    led = run.ledger()
    notes = run.journal().filter(pl.col("kind") == "note")["note"].to_list()
    assert notes and all(n.startswith("optimiser: Solved") and "binding: " in n for n in notes)
    assert any("sig.duration in [4, 4.5]" in n for n in notes), "the band binds: the best alphas are long"
    values = rec.frame().select("entity", "time", "value")
    times = sorted(set(run.journal().filter(pl.col("kind") == "note")["time"].to_list()))
    static = sig.group_by("isin").agg(pl.col("duration").first(), pl.col("sector").first()).rename({"isin": "entity"})
    for t in times:
        h = held.filter(pl.col("time") == t).join(values, on=["entity", "time"]).join(static, on="entity")
        nav = led.filter(pl.col("time") == t)["nav"][0]
        w = h["qty"] * h["value"] / nav
        assert w.sum() == pytest.approx(1.0, abs=1e-6) and w.max() <= 0.08 + 1e-6
        assert 4.0 - 1e-6 <= (w * h["duration"]).sum() <= 4.5 + 1e-6
        assert (h.with_columns(w=w).group_by("sector").agg(pl.col("w").sum())["w"] <= 0.3 + 1e-6).all()
    assert run.max_identity_error < 1e-9


def test_m6_an_infeasible_mandate_keeps_the_book_and_says_so_or_stops_the_run():
    ws, rec, _ = world()
    impossible = {"sig.duration": (20.0, 30.0)}
    run = ws.run(fi.Strategy(fi.Decision.optimise("sig.alpha", exposures=impossible), execution_lag=0, journal=True), rec)
    notes = run.journal().filter(pl.col("kind") == "note")["note"].to_list()
    assert run.trades == 0 and notes and all("infeasible" in n and "kept the book" in n for n in notes)
    with pytest.raises(fi.InvalidError, match="infeasible"):
        ws.run(fi.Strategy(fi.Decision.optimise("sig.alpha", exposures=impossible, on_infeasible="fail"), execution_lag=0), rec)
