"""The platform brief's acceptance cases, with the brief's own numbers.

The brief is private; each test names its case by the brief's identifiers:
- F01–F25: the structured-credit acceptance matrix;
- R1–R15: the report's representative fixtures;
- P1–P12: the procurement demonstrations.

A case that needs what the engine does not have yet is a strict xfail: it starts failing as a
reminder the day the change lands.
"""

from __future__ import annotations

import datetime as dt

import polars as pl
import pytest

import fi_engine as fi

D = [dt.date(2026, 1, 5) + dt.timedelta(days=i) for i in range(3)]


def bonds(marks: list[tuple], flows: dict | None = None, factor: bool = False) -> tuple[fi.Workspace, fi.Records]:
    """Bonds from marks rows (isin, date, clean, accrued[, factor]) and flows columns."""
    ws = fi.Workspace()
    names = ["isin", "date", "clean", "accrued"] + (["factor"] if factor else [])
    values = {"clean": "price", "accrued": "price"} | ({"factor": "number"} if factor else {})
    ws.series("marks", pl.DataFrame(marks, schema=names, orient="row"), entity="isin", kind="bond", time="date", values=values)
    if flows:
        f = pl.DataFrame(flows)
        cols = {c: "number" for c in f.columns if c not in ("isin", "date")}
        ws.series("flows", f, entity="isin", kind="bond", time="date", values=cols, nan_is_missing=True)
    contracts = ws.supplied_bonds("marks", flows="flows" if flows else None, factor="factor" if factor else None)
    return ws, ws.records(contracts, ws.calendar("marks"))


def units(rec: fi.Records, entity: str) -> pl.DataFrame:
    """One contract's unit records, in time order: value, cash and terms per unit."""
    return rec.frame().filter(pl.col("entity") == entity).sort("time")


def test_f06_principal_cash_is_15_not_17():
    """F06: a principal balance of 100 receives 5 scheduled and 10 prepaid principal, and 2 is
    written down without cash. End principal is 83 and principal cash 15: the lost principal is
    not cash."""
    ws, rec = bonds([("ABS", D[0], 100.0, 0.0), ("ABS", D[1], 100.0, 0.0)],
                    {"isin": ["ABS"], "date": [D[1]], "principal_face": [0.15], "writedown_face": [0.02]})
    r = units(rec, "ABS")
    assert r["value"].to_list() == pytest.approx([1.0, 0.83])
    assert r["cash"][1] == pytest.approx(0.15)
    assert r["principal"][1] == pytest.approx(-0.02), "the write-down is a loss"
    assert r["price"][1] == pytest.approx(0.0, abs=1e-15)


def test_f12_a_paydown_at_par_on_a_premium_pool_costs_the_premium():
    """F12: original face 1,000,000 at factor 0.80 and price 102 is worth 816,000. The factor falls
    to 0.75 with 50,000 of principal cash at an unchanged price: the pool is worth 765,000, and the
    economic change before interest is -1,000."""
    ws, rec = bonds([("POOL", D[0], 102.0, 0.0, 0.80), ("POOL", D[1], 102.0, 0.0, 0.75)],
                    {"isin": ["POOL"], "date": [D[1]], "principal_face": [0.05]}, factor=True)
    buy = fi.Decision.python(lambda ctx: fi.quantities({"POOL": 1_000_000.0}) if ctx.k == 0 else None)
    run = ws.run(fi.Strategy(buy, schedule="all", execution_lag=0), rec)
    led = run.ledger()
    assert (led["nav"] - led["cash"]).to_list() == pytest.approx([816_000.0, 765_000.0])
    assert led["cash"][1] - led["cash"][0] == pytest.approx(50_000.0)
    assert led["principal"][1] == pytest.approx(-1_000.0)
    assert led["nav"][1] - led["nav"][0] == pytest.approx(-1_000.0)
    assert run.max_identity_error < 1e-12


def test_p05_r2_repayment_and_writedown_cut_the_factor_alike_but_only_repayment_pays():
    """P05 and R2: two bonds at par each lose 20 of every 100 of face. The repaid one still
    represents 100 (80 of bond, 20 of cash); the written-down one has lost 20. A later write-up
    reinstates face without cash."""
    marks = [(b, d, 100.0, 0.0) for b in ("REP", "WD") for d in D]
    flows = {"isin": ["REP", "WD", "WD"], "date": [D[1], D[1], D[2]], "principal_face": [0.2, None, None],
             "writedown_face": [None, 0.2, None], "writeup_face": [None, None, 0.05]}
    ws, rec = bonds(marks, flows)
    rep, wd = units(rec, "REP"), units(rec, "WD")
    assert rep["value"].to_list() == pytest.approx([1.0, 0.8, 0.8]) and wd["value"].to_list() == pytest.approx([1.0, 0.8, 0.85])
    assert rep["value"][1] + rep["cash"][1] == pytest.approx(1.0), "the at-par control preserves combined value"
    assert rep["principal"].to_list() == pytest.approx([0.0, 0.0, 0.0])
    assert wd["cash"].to_list() == pytest.approx([0.0, 0.0, 0.0])
    assert wd["principal"].to_list() == pytest.approx([0.0, -0.2, 0.05])


def test_p10_r6_unfilled_quantities_stay_unfilled_and_the_next_decision_sees_them(synthetic):
    """P10 and R6, as far as today's core goes. A request nobody answers fills nothing, a size cap
    leaves the book short of its target, and each decision sees the actual book. Orders do not
    persist yet, so the decision restates its target; carried orders, expiry and baskets come with
    execution as a process."""
    ws, _, rec = synthetic
    bond = rec.contracts[0]
    seen = []

    def decide(ctx):
        seen.append(dict(ctx.holdings))
        return fi.quantities({bond: 1_000_000.0})

    silent = ws.run(fi.Strategy(fi.Decision.python(decide), fill=fi.Fill.rfq(response=0.0)), rec)
    # The last decision falls on the final day, so its order is still waiting when the run ends.
    assert silent.trades == 0 and silent.final_holdings == {} and silent.pending_at_end == 1
    assert silent.unfilled_orders == len(seen) - 1
    assert all(s == {} for s in seen)
    seen.clear()
    capped = ws.run(fi.Strategy(fi.Decision.python(decide), fill=fi.Fill.rfq(response=1.0, max_size=400_000.0)), rec)
    assert seen[:4] == [{}, {bond: 400_000.0}, {bond: 800_000.0}, {bond: 1_000_000.0}]
    assert capped.trades == 3 and capped.unfilled_orders == 2


def test_r4_futures_variation_margin_is_cash():
    """R4, as far as today's core goes: a daily-settled future is worth zero after each
    settlement, and its P&L is variation margin paid in cash each day. Contract rolls and residual
    exposure come with slices 1 and 2."""
    ws = fi.Workspace()
    prices = pl.DataFrame({"contract": ["TYH6"] * 3, "date": D, "price": [110.0, 110.5, 109.75]})
    ws.series("fut", prices, entity="contract", time="date", values={"price": "number"})
    rec = ws.records(ws.daily_future("TYH6", "fut.price", 1000.0), ws.calendar("fut"))
    buy = fi.Decision.python(lambda ctx: fi.quantities({"TYH6": 10.0}) if ctx.k == 0 else None)
    led = ws.run(fi.Strategy(buy, schedule="all", execution_lag=0), rec).ledger()
    margin = [0.0, 10 * 1000 * 0.5, 10 * 1000 * -0.75]
    assert led["cash"].diff().fill_null(0.0).to_list() == pytest.approx(margin)
    assert led["price"].to_list() == pytest.approx(margin), "the P&L is the margin"
    assert (led["nav"] - led["cash"]).abs().max() < 1e-9, "the position is worth zero after settlement"


def late_report(information: str, outcomes: str = "as_booked") -> list[float | None]:
    """A trustee report for February month end, released on 12 March and corrected on 14 March,
    read by decisions on 5, 13 and 16 March."""
    ws = fi.Workspace(information=information, outcomes=outcomes)
    report = pl.DataFrame({"deal": ["D", "D"], "period": [dt.date(2026, 2, 27)] * 2,
                           "available": [dt.datetime(2026, 3, 12), dt.datetime(2026, 3, 14)], "balance": [95.0, 94.0]})
    ws.series("trustee", report, entity="deal", time="period", known="available", values={"balance": "number"})
    days = [dt.date(2026, 3, 5), dt.date(2026, 3, 13), dt.date(2026, 3, 16)]
    ws.series("index", pl.DataFrame({"index": ["IDX"] * 3, "date": days, "level": [100.0] * 3}), entity="index", time="date",
              values={"level": "number"})
    rec = ws.records(ws.total_return("IDX", "index.level"), ws.calendar("index"))
    seen: list[float | None] = []
    ws.run(fi.Strategy(fi.Decision.python(lambda ctx: seen.append(ctx.value("trustee", "balance", "D"))), schedule="all"), rec)
    return seen


def test_f24_r15_p1_a_correction_is_seen_once_it_is_published():
    """F24, R15 and P1: a decision sees nothing before the release, the original from its release
    and the correction from its release. Restating outcomes does not change what decisions saw."""
    assert late_report("as_known") == [None, 95.0, 94.0]
    assert late_report("as_known", outcomes="restated") == [None, 95.0, 94.0]


def test_f24_the_other_information_policies_are_explicit():
    """First release never shows the correction. The legacy convention leaks it back to the
    original's release date, and says it is not point in time."""
    assert late_report("first_release") == [None, 95.0, 95.0]
    assert late_report("legacy_latest") == [None, 94.0, 94.0]


def test_f24_outcomes_as_booked_or_restated():
    """A mark published and later corrected: contracts step on the booked value unless the run
    restates, while decisions see each vintage from its publication."""
    def value(outcomes: str) -> float:
        ws = fi.Workspace(outcomes=outcomes)
        marks = pl.DataFrame({"isin": ["B"] * 3, "date": [D[0], D[1], D[1]], "clean": [100.0, 101.0, 99.0], "accrued": [0.0] * 3,
                              "published": [dt.datetime(2026, 1, 5, 18), dt.datetime(2026, 1, 6, 18), dt.datetime(2026, 1, 9)]})
        ws.series("marks", marks, entity="isin", kind="bond", time="date", known="published", values={"clean": "price", "accrued": "price"})
        info = ws.provenance("marks")
        assert info["knowledge"] == "observed" and info["revisions"]
        return units(ws.records(ws.supplied_bonds("marks"), ws.calendar("marks")), "B")["value"][1]

    assert value("as_booked") == pytest.approx(1.01)
    assert value("restated") == pytest.approx(0.99)
