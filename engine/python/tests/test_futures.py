"""Exchange-traded futures defined by Nautilus's FuturesContract, and a roll between them (review H5;
the brief's R4: the contract change, realised cash, variation margin and residual exposure
reconcile)."""

from __future__ import annotations

import datetime as dt

import numpy as np
import polars as pl
import pytest

import fi_engine as fi
from conftest import weekdays

D = dt.date.fromisoformat
CHAIN = [("TYH5", D("2025-03-20")), ("TYM5", D("2025-06-18")), ("TYU5", D("2025-09-19"))]
LISTED = {"TYU5": D("2025-03-03")}
TICK = 1 / 64


def market(days: int = 140, seed: int = 4):
    dates = weekdays(D("2025-01-02"), days)
    rng = np.random.default_rng(seed)
    front = 110 + np.cumsum(rng.normal(0, 0.2, days))
    rows = []
    for j, (name, expiry) in enumerate(CHAIN):
        for d, p in zip(dates, front - 0.25 * j, strict=True):
            if LISTED.get(name, dates[0]) <= d <= expiry:
                rows.append((name, d, float(p)))  # off the tick: the contract puts it on
    prices = pl.DataFrame(rows, schema=["contract", "date", "settle"], orient="row")
    ws = fi.Workspace()
    ws.series("futures", prices, entity="contract", kind="future", time="date", values={"settle": "price"})
    futures = None
    for name, expiry in CHAIN:
        f = ws.future(name, "futures.settle", expiration=expiry, activation=LISTED.get(name), multiplier=1000, price_increment="0.015625", underlying="ZN")
        futures = f if futures is None else futures + f
    rec = ws.records(futures, fi.Calendar(dates))
    return ws, rec, dates, prices


def test_r4_a_roll_changes_the_contract_and_margin_cash_and_exposure_reconcile():
    ws, rec, dates, prices = market()
    book = fi.Strategy(fi.Decision.roll(CHAIN, 10, days_before=5), schedule="all", execution_lag=0, fill=fi.Fill.research(0.0), initial_cash=1e6, currency="USD",
                       keep_holdings=True, journal=True)
    run = ws.run(book, rec)
    led = run.ledger()
    when = led["time"].dt.date().to_list()
    held = run.holdings()
    # The roll: the first time within five days of expiry, ten contracts move to the next one.
    roll = next(d for d in dates if (CHAIN[0][1] - d).days <= 5)
    at = held.with_columns(pl.col("time").dt.date()).filter(pl.col("time") == roll)
    assert dict(zip(at["entity"], at["qty"], strict=True)) == {"TYM5": 10.0}
    before = held.with_columns(pl.col("time").dt.date()).filter(pl.col("time") == dates[when.index(roll) - 1])
    assert dict(zip(before["entity"], before["qty"], strict=True)) == {"TYH5": 10.0}
    # Margin by hand: the held contract's change in settlement price, on the tick, × 1,000 × 10.
    on_tick = prices.with_columns((pl.col("settle") / TICK).round().mul(TICK).alias("p"))
    p = {(r["contract"], r["date"]): r["p"] for r in on_tick.iter_rows(named=True)}
    margin = 0.0
    for k in range(1, len(dates)):
        d0, d1 = dates[k - 1], dates[k]
        h = held.with_columns(pl.col("time").dt.date()).filter(pl.col("time") == d0)
        for name, q in zip(h["entity"], h["qty"], strict=True):
            margin += q * 1000 * (p[(name, d1)] - p[(name, d0)])
    assert led["cash"][-1] - 1e6 == pytest.approx(margin, abs=1e-6), "cash is the margin, exactly"
    assert led["price"].sum() == pytest.approx(margin, abs=1e-6) and led["nav"][-1] == pytest.approx(1e6 + margin, abs=1e-6)
    # No position is left in an expired contract, and the exposure is ten of the active one.
    final = held.with_columns(pl.col("time").dt.date()).filter(pl.col("time") == dates[-1])
    assert dict(zip(final["entity"], final["qty"], strict=True)) == {"TYU5": 10.0}
    assert run.max_identity_error < 1e-9
    fills = run.journal().filter(pl.col("kind") == "fill")
    assert fills.height == 5, "the first purchase, then two rolls of a sale and a purchase each"


def test_a_future_follows_its_nautilus_terms():
    ws, rec, dates, _ = market()
    frame = rec.frame()
    u = frame.filter(pl.col("entity") == "TYU5")
    listed = u.filter(pl.col("time").dt.date() < LISTED["TYU5"])
    assert listed["value"].is_nan().all(), "not listed yet: nothing to value or trade"
    h = frame.filter((pl.col("entity") == "TYH5") & (pl.col("time").dt.date() > CHAIN[0][1]))
    assert h["value"].to_list() == [0.0] * h.height and (h["price"] == 0.0).all(), "expired: no more margin"
    with pytest.raises(ValueError, match="Nautilus"):
        ws.future("TYH5", "futures.settle", expiration="2025-06-18", multiplier=0, price_increment="0.015625")
    with pytest.raises(ValueError, match="registry"):
        ws.future("TYH5", "futures.settle", expiration="2025-06-18", multiplier=1000, price_increment="0.015625", currency="XYZ")
