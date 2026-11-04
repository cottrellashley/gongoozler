"""Holding transitions (review H2) and the brief's demonstration P3: default, exchange and delayed
recovery, with extinguished and new claims, losses and cash each booked once, and the defaulted
securities kept as historical records."""

from __future__ import annotations

import datetime as dt

import polars as pl
import pytest

import fi_engine as fi
from conftest import weekdays

D = dt.date.fromisoformat
DEFAULT, LOAN_DEFAULT, RECOVERY, PAID, EXCHANGE = D("2025-02-03"), D("2025-02-10"), D("2025-03-03"), D("2025-03-24"), D("2025-04-01")

BONDS = {
    "OLD": fi.Bond("2020-03-01", "2030-03-01", 0.06, day_count="ACT/ACT ICMA", frequency=1),
    "LOAN": fi.Bond("2022-01-14", "2027-01-14", 0.08, day_count="ACT/360", frequency=4),
    "NEW": fi.Bond("2025-04-01", "2035-04-01", 0.03, day_count="30/360", frequency=2),
    "WARRANT": fi.Bond("2025-04-01", "2030-04-01", None, day_count="ACT/365F", frequency=0),
}


def world():
    dates = weekdays(D("2025-01-06"), 90)
    rows = []
    for d in dates:
        rows.append(("OLD", d, 60.0 if d < DEFAULT else 30.0))
        rows.append(("LOAN", d, 90.0 if d < LOAN_DEFAULT else 40.0))
        if d >= EXCHANGE:
            rows += [("NEW", d, 70.0), ("WARRANT", d, 20.0)]
    marks = pl.DataFrame(rows, schema=["isin", "date", "clean"], orient="row")
    events = pl.DataFrame(
        {
            "isin": ["OLD", "LOAN", "LOAN"],
            "date": [DEFAULT, LOAN_DEFAULT, RECOVERY],
            "default": [1.0, 1.0, None],
            "recovery": [None, None, 45.0],
            "recovery_delay": [None, None, float((PAID - RECOVERY).days)],
        }
    )
    ws = fi.Workspace()
    ws.series("marks", marks, entity="isin", kind="bond", time="date", values={"clean": "price"})
    ws.series("events", events, entity="isin", kind="bond", time="date", values={"default": "number", "recovery": "price", "recovery_delay": "number"})
    contracts = ws.debt(BONDS, fi.valuation.clean("marks.clean"), events="events")
    exchange = fi.transition(EXCHANGE, "OLD", {"NEW": 0.5, "WARRANT": 0.25}, cash=0.02, role="recovery", note="exchange offer")
    rec = ws.records(contracts, fi.Calendar(dates), transitions=[exchange])
    return ws, rec, dates


def buy(weights):
    return fi.Decision.python(lambda ctx: weights if ctx.k == 0 else None, name="buy")


def test_p3_default_exchange_and_delayed_recovery_book_every_claim_once():
    ws, rec, dates = world()
    run = ws.run(fi.Strategy(buy({"OLD": 0.5, "LOAN": 0.5}), schedule="all", execution_lag=0, fill=fi.Fill.research(0.0), journal=True), rec)
    led = run.ledger()
    when = led["time"].dt.date().to_list()
    at = when.index
    t0 = dates[0]
    v_old = 0.60 + BONDS["OLD"].accrued(t0)
    v_loan = 0.90 + BONDS["LOAN"].accrued(t0)
    q_old, q_loan = 5e7 / v_old, 5e7 / v_loan
    # The exchange: 0.30 of defaulted bond becomes 0.5 × 0.70 + 0.25 × 0.20 + 0.02 = 0.42, once.
    assert led["other"][at(EXCHANGE)] == pytest.approx(q_old * 0.12, rel=1e-9)
    assert led["cash"][at(EXCHANGE)] - led["cash"][at(EXCHANGE) - 1] == pytest.approx(q_old * 0.02, rel=1e-9)
    held = run.final_holdings
    assert "OLD" not in held and held["NEW"] == pytest.approx(q_old * 0.5, rel=1e-12) and held["WARRANT"] == pytest.approx(q_old * 0.25, rel=1e-12)
    # The loan's recovery, 45 against a mark of 40, is credit when determined; its cash pays three weeks later.
    assert led["other"][at(RECOVERY)] == pytest.approx(q_loan * 0.05, rel=1e-9)
    assert led["cash"][at(RECOVERY)] == pytest.approx(led["cash"][at(RECOVERY) - 1], abs=1e-6), "no cash yet"
    assert led["unsettled"][at(RECOVERY)] == pytest.approx(q_loan * 0.45, rel=1e-9)
    assert led["cash"][at(PAID)] - led["cash"][at(PAID) - 1] == pytest.approx(q_loan * 0.45, rel=1e-9)
    # Defaults write off the accrued interest bought; the loan paid one coupon before its default.
    cf = BONDS["LOAN"].cashflows()
    coupon = cf.filter((pl.col("kind") == "coupon") & (pl.col("pay") == D("2025-01-14")))["amount"][0]
    income_to_exchange = led["income"][: at(EXCHANGE) + 1].sum()
    expected = -q_old * BONDS["OLD"].accrued(t0) + q_loan * (coupon - BONDS["LOAN"].accrued(t0))
    assert income_to_exchange == pytest.approx(expected, rel=1e-9)
    # After the exchange the new bond accrues for its holder.
    assert led["income"][at(EXCHANGE) + 1 :].sum() == pytest.approx(q_old * 0.5 * BONDS["NEW"].accrued(dates[-1]), rel=1e-9)
    assert run.max_identity_error < 1e-9
    # Each claim leaves and arrives once.
    j = run.journal().filter(pl.col("kind") == "transition")
    assert j.height == 4 and sorted(j["entity"].drop_nulls().to_list()) == ["NEW", "OLD", "WARRANT"]


def test_defaulted_and_exchanged_securities_remain_records_and_every_book_sees_the_transition():
    ws, rec, dates = world()
    frame = rec.frame()
    old = frame.filter(pl.col("entity") == "OLD")
    assert old.height == len(dates), "the old bond is still a security that existed"
    new_before = frame.filter((pl.col("entity") == "NEW") & (pl.col("time").dt.date() < EXCHANGE))
    assert new_before["value"].is_nan().all(), "not issued yet: nothing to value, nothing to trade"
    runs = ws.sweep([fi.Strategy(buy({"OLD": w}), schedule="all", execution_lag=0) for w in (0.2, 0.6)], rec)
    assert [r.final_holdings["NEW"] / r.final_holdings["WARRANT"] for r in runs] == pytest.approx([2.0, 2.0])
    assert all(r.max_identity_error < 1e-9 for r in runs)


def test_transitions_that_cannot_be_applied_are_refused():
    ws, _, dates = world()
    contracts = ws.debt(BONDS, fi.valuation.clean("marks.clean"), events="events")
    cal = fi.Calendar(dates)
    with pytest.raises(KeyError, match="NOPE"):
        ws.records(contracts, cal, transitions=[fi.transition(EXCHANGE, "OLD", {"NOPE": 1.0})])
    with pytest.raises(fi.InvalidError, match="first time"):
        ws.records(contracts, cal, transitions=[fi.transition(dates[0], "OLD", {"NEW": 1.0})])
    with pytest.raises(fi.InvalidError, match="order would decide"):
        ws.records(contracts, cal, transitions=[fi.transition(EXCHANGE, "OLD", {"NEW": 1.0}), fi.transition(EXCHANGE, "NEW", {"WARRANT": 1.0})])
    with pytest.raises(fi.InvalidError, match="twice"):
        ws.records(contracts, cal, transitions=[fi.transition(EXCHANGE, "OLD", {"NEW": 1.0}), fi.transition(dates[-1], "OLD", cash=0.1)])
    early = ws.records(contracts, cal, transitions=[fi.transition(dates[5], "OLD", {"NEW": 1.0})])
    with pytest.raises(fi.MissingError, match="cannot be valued"):
        ws.run(fi.Strategy(buy({"OLD": 0.5}), schedule="all", execution_lag=0), early)
