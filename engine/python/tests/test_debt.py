"""Debt from its terms (review C1–C3): conventions, cash flows, analytics, and the ledger's books of
coupons, ex-coupon entitlement, calls and redemptions, each against a hand calculation."""

from __future__ import annotations

import datetime as dt

import numpy as np
import polars as pl
import pytest

import fi_engine as fi
from conftest import weekdays


def d(s: str) -> dt.date:
    return dt.date.fromisoformat(s)


def flat_marks(name: str, start: dt.date, n: int, clean: float = 100.0) -> pl.DataFrame:
    dates = weekdays(start, n)
    return pl.DataFrame({"isin": [name] * n, "date": dates, "clean": [clean] * n})


def hold_then(sell_at: int | None = None):
    """Buys the whole book at the first time; sells it all at step `sell_at`, if given."""

    @fi.decision
    def decide(ctx):
        if ctx.k == 0:
            return {ctx.contracts[0]: 1.0}
        if sell_at is not None and ctx.k == sell_at:
            return fi.quantities({ctx.contracts[0]: 0.0})
        return None

    return decide


# ------------------------------------------------------------------------------- terms and flows


def test_a_fixed_bond_pays_what_a_hand_calculation_says():
    b = fi.Bond("2024-01-15", "2027-01-15", 0.05, day_count="30/360", frequency=2)
    cf = b.cashflows()
    coupons = cf.filter(pl.col("kind") == "coupon")
    assert coupons.height == 6 and coupons["amount"].to_list() == pytest.approx([0.025] * 6, abs=1e-15)
    red = cf.filter(pl.col("kind") == "redemption")
    assert red["amount"].to_list() == [1.0] and red["pay"].to_list() == [d("2027-01-15")]
    assert b.accrued("2024-04-15") == pytest.approx(0.05 * 90 / 360, abs=1e-15)
    # At a yield equal to its coupon, on a coupon date, a bond is at par.
    p = b.price("2024-07-15", yield_=0.05)
    assert p["accrued"] == pytest.approx(0.0, abs=1e-12) and p["clean"] == pytest.approx(100.0, abs=1e-9)
    y = b.yield_("2024-09-03", b.price("2024-09-03", yield_=0.045)["clean"])
    assert y == pytest.approx(0.045, abs=1e-10)


def test_act_act_icma_accrues_over_the_coupon_period():
    b = fi.Bond("2024-02-15", "2034-02-15", 0.04, day_count="ACT/ACT ICMA", frequency=2)
    # 90 days of a 182-day period, at half the annual coupon.
    assert b.accrued("2024-05-15") == pytest.approx(0.02 * 90 / 182, abs=1e-15)
    assert b.frequency == 2 and b.day_count == "ACT/ACT ICMA"


def test_holidays_come_from_declared_rules_and_move_payment_dates():
    cal = fi.BusinessCalendar.from_rules("X", [("fixed", 12, 25, "nearest"), ("easter", -2), ("easter", 1)], (2022, 2026))
    assert not cal.is_business_day("2022-12-26"), "Christmas on a Sunday is observed on the Monday"
    assert not cal.is_business_day("2024-03-29") and not cal.is_business_day("2024-04-01"), "Good Friday and Easter Monday 2024"
    assert cal.adjust("2024-03-30", "following") == d("2024-04-02")
    assert cal.adjust("2024-03-30", "modified_following") == d("2024-03-28"), "following would leave the month"
    assert cal.add_business_days("2024-03-28", 1) == d("2024-04-02")
    # A coupon scheduled on Good Friday pays the next business day, and goes ex seven business days before.
    b = fi.Bond("2023-03-29", "2025-03-29", 0.03, day_count="ACT/ACT ICMA", frequency=1, calendar=cal, ex_days=7)
    first = b.periods().row(0, named=True)
    assert first["end"] == d("2024-03-29") and first["pay"] == d("2024-04-02")
    assert first["ex"] == cal.add_business_days("2024-04-02", -7)


def test_an_amortiser_accrues_on_what_is_outstanding():
    b = fi.Bond("2024-01-01", "2026-01-01", 0.06, day_count="30/360", frequency=2, amortisation=[("2025-01-01", 0.5, 100.0)])
    cf = b.cashflows()
    assert cf.filter(pl.col("kind") == "principal")["amount"].to_list() == [0.5]
    assert cf.filter(pl.col("kind") == "coupon")["amount"].to_list() == pytest.approx([0.03, 0.03, 0.015, 0.015], abs=1e-15)
    assert b.balance("2025-06-30") == 0.5


def test_a_floater_fixes_in_advance_and_refuses_to_guess_a_missing_fixing():
    cal = fi.BusinessCalendar.weekends()
    b = fi.Bond("2024-01-02", "2025-01-02", fi.Coupon.floating("TERM", spread=0.005, lag=2), day_count="ACT/360", frequency=4)
    starts = [r["start"] for r in b.periods().iter_rows(named=True)]
    fixings = {"TERM": {cal.add_business_days(s, -2): 0.04 + 0.001 * i for i, s in enumerate(starts)}}
    cf = b.cashflows(fixings).filter(pl.col("kind") == "coupon")
    days = [(e - s).days for s, e in zip(cf["accrual_start"], cf["accrual_end"], strict=True)]
    assert cf["amount"].to_list() == pytest.approx([(0.045 + 0.001 * i) * n / 360 for i, n in enumerate(days)], abs=1e-15)
    del fixings["TERM"][cal.add_business_days(starts[2], -2)]
    with pytest.raises(fi.MissingError):
        b.cashflows(fixings)
    # A forecast curve projects what was not observed; what was observed stays.
    projected = b.cashflows(fixings, forecast=fi.Curve.flat("2024-01-02", 0.04)).filter(pl.col("kind") == "coupon")
    assert projected["amount"][0] == pytest.approx(cf["amount"][0], abs=1e-15)


def test_terms_that_contradict_themselves_are_refused():
    with pytest.raises(fi.InvalidError):
        fi.Bond("2025-01-01", "2024-01-01", 0.05, day_count="30/360")
    with pytest.raises(fi.InvalidError):
        fi.Bond("2024-01-01", "2026-01-01", 0.05, day_count="30/360", amortisation=[("2025-01-01", 1.5, 100.0)])
    with pytest.raises(fi.InvalidError):
        fi.Bond("2024-01-01", "2026-01-01", 0.05, day_count="ACT/999")
    with pytest.raises(ValueError):
        fi.Bond("2024-01-01", "2026-01-01", 0.05, day_count="30/360", frequency=5)


# ------------------------------------------------------------------------------------- analytics


def test_risk_carries_its_definition_and_is_the_difference_it_says():
    b = fi.Bond("2024-07-15", "2031-07-15", 0.045, day_count="30/360", frequency=2)
    curve = fi.Curve("2024-09-03", [1.0, 2.0, 5.0, 10.0], [0.03, 0.032, 0.035, 0.037])
    settle = "2024-09-03"
    up, down = b.price(settle, curve=curve.shifted(1.0)), b.price(settle, curve=curve.shifted(-1.0))
    r = b.risk(settle, curve)
    assert r["value"] == pytest.approx((down["dirty"] - up["dirty"]) / 2, rel=1e-12)
    assert "per 1bp fall" in r["definition"] and "central difference" in r["definition"]
    keys = sum(b.risk(settle, curve, bump=i)["value"] for i in range(4))
    assert keys == pytest.approx(r["value"], rel=1e-6), "key rates add up to the parallel bump"
    clean = b.price(settle, curve=curve, spread=0.0125)["clean"]
    assert b.z_spread(settle, clean, curve) == pytest.approx(0.0125, abs=1e-10)
    dur = b.durations(settle, 0.04)
    assert 5.0 < dur["modified"] < dur["macaulay"] < 7.0 and dur["convexity"] > 0


# ------------------------------------------------------------------------------------ the ledger


def run_one(bond: fi.Bond, marks: pl.DataFrame, decide, events: pl.DataFrame | None = None, valuation=None):
    ws = fi.Workspace()
    ws.series("marks", marks, entity="isin", kind="bond", time="date", values={c: "price" for c in marks.columns if c not in ("isin", "date")}, grid=True)
    if events is not None:
        ws.series("events", events, entity="isin", kind="bond", time="date", values={c: "number" for c in events.columns if c not in ("isin", "date")})
    contracts = ws.debt({"B": bond}, valuation or fi.valuation.clean("marks.clean"), events="events" if events is not None else None)
    rec = ws.records(contracts, ws.calendar("marks"))
    return ws.run(fi.Strategy(decide, schedule="all", execution_lag=0, fill=fi.Fill.research(0.0)), rec)


def test_a_bond_held_to_maturity_books_its_coupons_as_income_and_its_redemption_at_par():
    b = fi.Bond("2024-01-02", "2024-07-02", 0.06, day_count="ACT/360", frequency=4)
    run = run_one(b, flat_marks("B", d("2024-01-02"), 140), hold_then())
    led = run.ledger()
    coupon = 0.06 * 91 / 360  # both quarters have 91 days
    q = 1e8  # bought at par with no accrued interest
    assert led["income"].sum() == pytest.approx(2 * q * coupon, rel=1e-12)
    assert led["principal"].abs().max() < 1e-6 and led["price"].abs().max() < 1e-6
    assert run.final_nav == pytest.approx(q * (1 + 2 * coupon), rel=1e-12)
    dates = led["time"].dt.date().to_list()
    i = dates.index(d("2024-04-02"))
    assert led["cash"][i] - led["cash"][i - 1] == pytest.approx(q * coupon, rel=1e-12), "the coupon is cash on its date"
    assert run.max_identity_error < 1e-9


def test_a_coupon_gone_ex_is_the_sellers_and_pays_on_its_date():
    """Sold two days after it goes ex, the bond trades at its negative accrued interest, and the
    coupon still pays to the seller on the payment date (the brief's first sovereign case)."""
    b = fi.Bond("2024-01-02", "2024-07-02", 0.06, day_count="ACT/360", frequency=4, ex_days=5)
    marks = flat_marks("B", d("2024-01-02"), 80)
    dates = marks["date"].to_list()
    ex = b.periods()["ex"][0]
    sell = dates.index(ex) + 2
    run = run_one(b, marks, hold_then(sell))
    led = run.ledger()
    q = 1e8
    earned = 0.06 * (dates[sell] - d("2024-01-02")).days / 360
    assert b.accrued(dates[sell]) == pytest.approx(earned - 0.06 * 91 / 360, abs=1e-15), "negative once ex"
    assert led["income"].sum() == pytest.approx(q * earned, rel=1e-12)
    assert run.final_nav == pytest.approx(q * (1 + earned), rel=1e-12)
    pay = dates.index(d("2024-04-02"))
    assert led["cash"][pay] - led["cash"][pay - 1] == pytest.approx(q * 0.06 * 91 / 360, rel=1e-12), "the coupon arrives after the sale"
    assert run.max_identity_error < 1e-9


def test_an_observed_call_pays_its_price_and_its_accrued_and_the_premium_is_principal():
    b = fi.Bond("2024-01-02", "2029-01-02", 0.05, day_count="30/360", frequency=2, calls=[("2024-03-01", "2028-12-31", 101.0)])
    marks = flat_marks("B", d("2024-01-02"), 120)
    called = d("2024-04-01")
    events = pl.DataFrame({"isin": ["B"], "date": [called], "call": [101.0]})
    run = run_one(b, marks, hold_then(), events)
    led = run.ledger()
    q = 1e8
    accrued = 0.05 * 89 / 360  # 2 January to 1 April, 30/360
    assert led["principal"].sum() == pytest.approx(q * 0.01, rel=1e-9)
    assert led["income"].sum() == pytest.approx(q * accrued, rel=1e-9)
    assert run.final_nav == pytest.approx(q * (1.01 + accrued), rel=1e-12)
    assert run.max_identity_error < 1e-9


def test_a_call_outside_the_terms_is_refused():
    b = fi.Bond("2024-01-02", "2029-01-02", 0.05, day_count="30/360", frequency=2, calls=[("2026-01-02", "2028-12-31", 100.0)])
    events = pl.DataFrame({"isin": ["B"], "date": [d("2024-04-01")], "call": [100.0]})
    with pytest.raises(fi.EngineError, match="call"):
        run_one(b, flat_marks("B", d("2024-01-02"), 120), hold_then(), events)


def test_a_yield_values_the_holding_through_its_terms():
    b = fi.Bond("2024-01-15", "2034-01-15", 0.04, day_count="30/360", frequency=2)
    marks = flat_marks("B", d("2024-03-01"), 30).with_columns(pl.lit(0.045).alias("ytm")).drop("clean")
    run = run_one(b, marks, hold_then(), valuation=fi.valuation.yield_("marks.ytm"))
    led = run.ledger()
    first, last = marks["date"][0], marks["date"][-1]
    p0, p1 = b.price(first, yield_=0.045), b.price(last, yield_=0.045)
    q = 1e8 / (p0["dirty"] / 100)
    assert run.final_nav == pytest.approx(q * p1["dirty"] / 100, rel=1e-12)
    assert led["income"].sum() == pytest.approx(q * (b.accrued(last) - b.accrued(first)), rel=1e-9)
    assert run.max_identity_error < 1e-9


def test_bonds_from_a_terms_frame():
    terms = pl.DataFrame(
        {
            "name": ["FIX", "FRN", "CALL"],
            "issue": [d("2024-01-02")] * 3,
            "maturity": [d("2029-01-02")] * 3,
            "day_count": ["30/360", "ACT/360", "30/360"],
            "frequency": [2, 4, 2],
            "coupon": [0.05, None, 0.06],
            "index": [None, "TERM", None],
            "spread": [None, 0.0075, None],
            "calendar": ["X", "X", None],
        }
    )
    rights = pl.DataFrame({"name": ["CALL"], "kind": ["call"], "from": [d("2027-01-02")], "to": [d("2028-12-31")], "price": [100.0]})
    out = fi.bonds(terms, calendars={"X": fi.BusinessCalendar.weekends()}, rights=rights)
    assert sorted(out) == ["CALL", "FIX", "FRN"]
    assert "TERM + 75bp" in repr(out["FRN"]) and out["FRN"].frequency == 4
    assert out["FIX"].cashflows().height == 11
    with pytest.raises(ValueError, match="calendar"):
        fi.bonds(terms, calendars={})


# ------------------------------------------------------------------- supplied histories (review H1)


def supplied(accrued: list[float], coupon_day: int, coupon: float, every: int = 1):
    days = len(accrued)
    dates = weekdays(d("2025-01-06"), days)
    ws = fi.Workspace()
    ws.series("marks", pl.DataFrame({"isin": ["B"] * days, "date": dates, "clean": [100.0] * days, "accrued": accrued}),
              entity="isin", kind="bond", time="date", values={"clean": "price", "accrued": "price"}, grid=True)
    ws.series("flows", pl.DataFrame({"isin": ["B"], "date": [dates[coupon_day]], "interest": [coupon / 100]}),
              entity="isin", kind="bond", time="date", values={"interest": "number"})
    cal = fi.Calendar(dates[::every] if (days - 1) % every == 0 else [*dates[::every], dates[-1]])
    return ws, cal


def test_supplied_accrued_interest_that_never_falls_back_is_refused():
    """The review's probe: accrued climbing through a coupon would book the coupon twice."""
    ws, cal = supplied([0.01 * k for k in range(60)], 30, 0.30)
    with pytest.raises(fi.InvalidError, match="count twice"):
        ws.records(ws.supplied_bonds("marks", flows="flows"), cal)
    rec = ws.records(ws.supplied_bonds("marks", flows="flows", reconcile=False), cal)
    assert len(rec) == 1, "an explicit opt-out for data the check cannot judge"


def test_supplied_accrued_interest_that_resets_reconciles_on_coarse_steps_and_ex_coupon():
    ws, cal = supplied([0.01 * (k % 30) for k in range(60)], 30, 0.30)
    ws.records(ws.supplied_bonds("marks", flows="flows"), cal)
    # Steps of a month (20 weekdays) across a coupon: the accrued interest is compared with its
    # level after the last coupon, not across one step.
    ws, cal = supplied([0.01 * (k % 30) for k in range(61)], 30, 0.30, every=20)
    ws.records(ws.supplied_bonds("marks", flows="flows"), cal)
    # Ex-coupon five days before payment: negative accrued, then the coupon pays.
    acc = [0.01 * k if k < 25 else 0.01 * (k - 30) for k in range(60)]
    ws, cal = supplied(acc, 30, 0.30)
    ws.records(ws.supplied_bonds("marks", flows="flows"), cal)


def test_coupons_round_as_the_terms_say_and_book_exactly():
    """Review H4: a contractual amount is the decimal its terms define, and a book's flow is its
    quantity times that, in decimal: 1e8 of face receives 1,516,670.00, not a binary neighbour."""
    b = fi.Bond("2024-01-02", "2024-07-02", 0.06, day_count="ACT/360", frequency=4, rounding=7)
    cf = b.cashflows().filter(pl.col("kind") == "coupon")
    assert cf["amount"].to_list() == [0.0151667, 0.0151667]
    run = run_one(b, flat_marks("B", d("2024-01-02"), 70), hold_then())
    led = run.ledger()
    dates = led["time"].dt.date().to_list()
    i = dates.index(d("2024-04-02"))
    assert led["cash"][i] - led["cash"][i - 1] == 1_516_670.0


# ---------------------------------------------------------------------------- attribution (M7)


def curve_world(days: int = 60):
    """Two bonds off a flat curve and a spread each: the curve moves for the first 20 days, the
    spreads for the next 20, nothing for the last 20."""
    dates = weekdays(d("2025-03-03"), days)
    k = np.arange(days)
    level = 0.04 + np.where(k < 20, 0.0002 * k, 0.0002 * 19)
    spread = 0.01 + np.where(k < 20, 0.0, np.where(k < 40, 0.0001 * (k - 19), 0.0001 * 20))
    ws = fi.Workspace()
    ws.series("curve", pl.DataFrame({"curve": "GOV", "date": dates, "z1": level, "z10": level, "z30": level}), entity="curve", kind="curve", time="date",
              values={"z1": "number", "z10": "number", "z30": "number"}, grid=True)
    ws.series("spreads", pl.DataFrame({"isin": ["A"] * days + ["B"] * days, "date": dates * 2, "spread": np.r_[spread, spread * 1.5]}),
              entity="isin", kind="bond", time="date", values={"spread": "number"}, grid=True)
    bonds = {"A": fi.Bond("2024-01-15", "2034-01-15", 0.045, day_count="30/360", frequency=2),
             "B": fi.Bond("2024-06-01", "2029-06-01", 0.05, day_count="30/360", frequency=2)}
    contracts = ws.debt(bonds, fi.valuation.curve("curve", "GOV", {"z1": 1.0, "z10": 10.0, "z30": 30.0}, spread="spreads.spread"))
    return ws, contracts, fi.Calendar(dates), dates, bonds


def test_m7_a_curve_valued_bonds_pnl_splits_into_carry_curve_and_spread():
    ws, contracts, cal, dates, bonds = curve_world()
    x = ws.explain(contracts, cal)
    assert (x["carry"] + x["curve"] + x["spread"] + x["residual"] - x["total"]).abs().max() < 1e-12
    assert x["residual"].abs().max() < 1e-12, "a curve-valued bond is fully explained"
    a = x.filter(pl.col("entity") == "A").with_columns(pl.col("time").dt.date())
    curve_phase, spread_phase, still = a[:18], a[20:38], a[41:]
    assert spread_phase["curve"].abs().max() < 1e-12 and curve_phase["spread"].abs().max() < 1e-12, "each moves alone"
    assert (curve_phase["curve"] < 0).all() and (spread_phase["spread"] < 0).all(), "rates and spreads rising lose"
    assert (still["curve"].abs().max(), still["spread"].abs().max()) < (1e-12, 1e-12) and (still["carry"] > 0).all()
    # A two-basis-point rise of the curve costs about twice the bond's DV01 (per unit: DV01 / 100).
    settle, curve = dates[4], fi.Curve(dates[4], [1.0, 10.0, 30.0], [0.04 + 0.0002 * 4] * 3)
    dv01 = bonds["A"].risk(settle, curve, spread=0.01)["value"] / 100
    assert a.filter(pl.col("time") == dates[5])["curve"][0] == pytest.approx(-2 * dv01, rel=0.02)


def test_m7_a_books_pnl_is_explained_and_adds_up_to_navs_change():
    ws, contracts, cal, dates, _ = curve_world()
    rec = ws.records(contracts, cal)
    run = ws.run(fi.Strategy(hold_then(), schedule="all", execution_lag=0, fill=fi.Fill.research(0.0), keep_holdings=True), rec)
    att = fi.attribution(run, ws.explain(contracts, cal))
    parts = att["carry"] + att["curve"] + att["spread"] + att["unexplained"] + att["book"]
    assert (parts - att["pnl"]).abs().max() < 1e-6
    assert att["unexplained"].abs().max() < 1e-6 and att["book"].abs().max() < 1e-6, "no cash interest, no costs: all of it is the bond's"
    assert att["curve"][:20].sum() < 0 < att["carry"].sum()


# ------------------------------------------------------------------------- auctions and primary (M5)


def test_m5_a_bid_list_sells_some_lines_whole_and_the_rest_do_not_trade():
    bonds = {f"L{i:02d}": fi.Bond("2024-01-15", "2030-01-15", 0.05, day_count="30/360", frequency=2) for i in range(20)}
    dates = weekdays(d("2025-02-03"), 10)
    marks = pl.DataFrame({"isin": [n for n in bonds for _ in dates], "date": dates * len(bonds), "clean": 98.0})
    ws = fi.Workspace()
    ws.series("marks", marks, entity="isin", kind="bond", time="date", values={"clean": "price"}, grid=True)
    rec = ws.records(ws.debt(bonds, fi.valuation.clean("marks.clean")), ws.calendar("marks"))
    names = sorted(bonds)

    @fi.decision
    def list_them(ctx):
        return {n: 0.0 for n in names} if ctx.k == 3 else None

    # The book holds every line, and puts them all on one list.
    auction = fi.Fill.auction(bidders=3, participation=0.5, discount_bp=10, dispersion_bp=40, reserve_bp=30, seed=4)
    book = fi.Book([fi.Policy(list_them, schedule="all", lag=0)], execution=auction, initial_cash=0.0, opening=[(n, 5e6) for n in names],
                   time_in_force=1, journal=True)
    run = ws.run(book, rec)
    sold = run.journal().filter((pl.col("kind") == "fill") & (pl.col("qty") < 0))
    left = run.final_holdings
    assert 0 < sold.height < 20 and len(left) == 20 - sold.height, "lines trade whole or not at all"
    values = rec.frame().select("entity", "time", "value")
    for row in sold.join(values, on=["entity", "time"]).iter_rows(named=True):
        price, v = -row["amount"] / row["qty"], row["value"]  # a unit's value is dirty: clean plus accrued
        assert v * (1 - 30e-4) - 1e-12 <= price <= v * (1 - 10e-4) + 1e-12, "within the reserve"


def test_m5_a_new_issue_is_allocated_its_fraction_at_the_reoffer():
    new = fi.Bond("2025-02-10", "2030-02-10", 0.05, day_count="30/360", frequency=2)
    dates = weekdays(d("2025-02-03"), 15)
    marks = pl.DataFrame({"isin": ["NEW"] * 10, "date": dates[5:], "clean": 100.25})
    ws = fi.Workspace()
    ws.series("marks", marks, entity="isin", kind="bond", time="date", values={"clean": "price"})
    rec = ws.records(ws.debt({"NEW": new}, fi.valuation.clean("marks.clean")), fi.Calendar(dates))

    @fi.decision
    def bid(ctx):
        return fi.orders({"NEW": 1e8}) if ctx.t == rec.calendar.times()[5] else None

    run = ws.run(fi.Book([fi.Policy(bid, schedule="all", lag=0)], execution=fi.Fill.primary({"NEW": (99.5, 0.4)}), initial_cash=1e8), rec)
    assert run.final_holdings["NEW"] == pytest.approx(4e7)
    assert run.final_cash[next(iter(run.final_cash))] == pytest.approx(1e8 - 4e7 * 0.995, rel=1e-12), "paid the reoffer"
    assert run.max_identity_error < 1e-9
