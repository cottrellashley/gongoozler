"""The platform brief's acceptance cases that waited for debt from its terms: F12 (an agency pool's
factors, a flash estimate superseded), F19 (a covered bond's soft-bullet extension and dual
recourse) and P8 (a call, a conversion, inflation and floating fixings revised, and floors). Each
test names its case by the brief's identifier; the brief is private."""

from __future__ import annotations

import datetime as dt

import polars as pl
import pytest

import fi_engine as fi
from conftest import weekdays

D = dt.date.fromisoformat


def run_debt(bonds, marks, events=None, *, outcomes="as_booked", fixings=None, decide=None, transitions=None, extra=None, journal=False):
    ws = fi.Workspace(outcomes=outcomes)
    ws.series("marks", marks, entity="isin", kind="bond", time="date", values={"clean": "price"})
    if events is not None:
        cols = {c: "number" for c in events.columns if c not in ("isin", "date", "published")}
        ws.series("events", events, entity="isin", kind="bond", time="date", values=cols, known="published" if "published" in events.columns else None)
    if fixings is not None:
        ws.series("fixings", fixings, entity="index", kind="index", time="date", known="published" if "published" in fixings.columns else None,
                  values={"rate": "number"})
    contracts = ws.debt(bonds, fi.valuation.clean("marks.clean"), events="events" if events is not None else None,
                        fixings="fixings.rate" if fixings is not None else None)
    if extra is not None:
        contracts = contracts + extra(ws)
    rec = ws.records(contracts, fi.Calendar(sorted(set(marks["date"].to_list()))), transitions=transitions)
    buy = decide or fi.Decision.python(lambda ctx: fi.quantities({n: 1_000_000.0 for n in bonds}) if ctx.k == 0 else None)
    return ws, rec, ws.run(fi.Strategy(buy, schedule="all", execution_lag=0, fill=fi.Fill.research(0.0), journal=journal), rec)


def test_f12_a_pools_factor_flash_then_official_and_the_premium_it_costs():
    """F12: 1,000,000 of original face at factor 0.80 and 102 is worth 816,000; the factor falls to
    0.75 with 50,000 of principal at an unchanged price: 765,000, a premium effect of −1,000. The
    flash estimate (0.76) is known before the official figure; decisions see each as published, and
    the accounts restated on the official one."""
    dates = weekdays(D("2025-02-03"), 6)
    pool = fi.Bond("2020-01-01", "2050-01-01", 0.0, day_count="30/360", frequency=12)
    marks = pl.DataFrame({"isin": "POOL", "date": dates, "clean": 102.0})
    events = pl.DataFrame({
        "isin": "POOL", "date": [D("2025-01-15"), dates[2], dates[2]], "factor": [0.80, 0.76, 0.75],
        "published": [dt.datetime(2025, 1, 15, 12), dt.datetime(2025, 2, 5, 12), dt.datetime(2025, 2, 7, 12)],
    })
    seen = []

    @fi.decision
    def watch(ctx):
        seen.append(ctx.value("events", "factor", "POOL"))
        return fi.quantities({"POOL": 1_000_000.0}) if ctx.k == 0 else None

    _, _, run = run_debt({"POOL": pool}, marks, events, outcomes="restated", decide=watch)
    led = run.ledger()
    assert (led["nav"] - led["cash"])[0] == pytest.approx(816_000.0) and (led["nav"] - led["cash"])[2] == pytest.approx(765_000.0)
    assert led["cash"][2] - led["cash"][1] == pytest.approx(50_000.0)
    assert (led["price"] + led["principal"])[2] == pytest.approx(-1_000.0), "the premium a paydown at par costs"
    assert seen[2:6] == pytest.approx([0.80, 0.76, 0.76, 0.75]), "nothing before its publication: the flash at noon, the official two days on"
    assert run.max_identity_error < 1e-12


def test_f19_a_soft_bullet_extends_with_no_par_cash_and_dual_recourse_never_recovers_twice():
    """F19: the issuer fails to pay at the scheduled maturity, so the bond extends: no par cash then,
    a new coupon after. Its claim is then recovered through two routes, the issuer's estate and the
    cover pool, settling parts of one claim; a third recovery is refused."""
    cb = fi.Bond("2020-03-02", "2025-03-03", 0.02, day_count="ACT/ACT ICMA", frequency=1, extension=("2026-03-02", fi.Coupon.fixed(0.035)))
    dates = weekdays(D("2025-02-24"), 120)
    marks = pl.DataFrame({"isin": "CB", "date": dates, "clean": 99.0})
    maturity, failed, estate, pool = D("2025-03-03"), D("2025-05-05"), D("2025-06-02"), D("2025-07-01")
    events = pl.DataFrame({"isin": "CB", "date": [maturity, failed, estate, pool], "extend": [1.0, None, None, None], "default": [None, 1.0, None, None],
                           "recovery": [None, None, 100.0, 100.0], "recovered": [None, None, 0.30, 0.55]})
    _, rec, run = run_debt({"CB": cb}, marks, events)
    led = run.ledger()
    when = led["time"].dt.date().to_list()
    at = when.index
    coupon = 0.02 * 1e6
    assert led["cash"][at(maturity)] - led["cash"][at(maturity) - 1] == pytest.approx(coupon), "the scheduled coupon, and no par"
    accrued = rec.frame().filter(pl.col("entity") == "CB")
    assert accrued["value"][at(failed) - 1] > 0.99, "still a claim on the extended terms, accruing 3.5%"
    assert led["cash"][at(estate)] - led["cash"][at(estate) - 1] == pytest.approx(300_000.0)
    assert led["cash"][at(pool)] - led["cash"][at(pool) - 1] == pytest.approx(550_000.0)
    assert run.max_identity_error < 1e-9
    extended = cb.cashflows().filter(pl.col("kind") == "redemption")["date"].to_list()
    assert extended == [maturity], "the terms' own redemption is the scheduled one; the extension is an event"
    def third(recovered: float) -> pl.DataFrame:
        row = pl.DataFrame({"isin": ["CB"], "date": [D("2025-07-15")], "extend": [None], "default": [None], "recovery": [100.0], "recovered": [recovered]},
                           schema=events.schema)
        return events.vstack(row)

    run_debt({"CB": cb}, marks, third(0.15))  # the rest of the claim: allowed
    with pytest.raises(fi.InvalidError, match="settles 0.5 of face, but the claim has"):
        run_debt({"CB": cb}, marks, third(0.5))


def test_p08_a_call_a_conversion_revised_fixings_and_floors():
    """P8: a callable bond called at its price; a convertible converted into shares, the exchange
    booked once; an inflation-linked bond's coupon on the index as booked or as revised; and a
    floater floored at zero when its fixing goes negative."""
    dates = weekdays(D("2025-01-06"), 80)
    called, converted = D("2025-02-03"), D("2025-03-03")
    bonds = {
        "CALL": fi.Bond("2023-01-15", "2033-01-15", 0.06, day_count="30/360", frequency=2, calls=[("2025-01-15", "2032-12-31", 101.0)]),
        "CONV": fi.Bond("2023-06-01", "2028-06-01", 0.01, day_count="30/360", frequency=2),
        "FRN": fi.Bond("2024-12-02", "2026-12-02", fi.Coupon.floating("TERM", spread=0.002, floor=0.0, lag=2), day_count="ACT/360", frequency=4),
    }
    marks = pl.DataFrame({"isin": [n for n in bonds for _ in dates], "date": dates * 3, "clean": 100.0})
    events = pl.DataFrame({"isin": ["CALL"], "date": [called], "call": [101.0]})
    fixings = pl.DataFrame({"index": "TERM", "date": [D("2024-11-28"), D("2025-02-27")], "rate": [-0.005, -0.004]})

    def shares(ws):
        ws.series("equity", pl.DataFrame({"ticker": "EQ", "date": dates, "price": 30.0}), entity="ticker", time="date", values={"price": "number"})
        return ws.total_return("EQ", "equity.price")

    conversion = fi.transition(converted, "CONV", {"EQ": 0.04}, role="principal", note="converted: 40 shares per 1,000")
    _, _, run = run_debt(bonds, marks, events, fixings=fixings, extra=shares, transitions=[conversion], journal=True)
    led = run.ledger()
    at = led["time"].dt.date().to_list().index
    assert led["principal"][at(called)] == pytest.approx(1e6 * 0.01, rel=1e-9), "called at 101: the premium is principal"
    assert run.final_holdings["EQ"] == pytest.approx(40_000.0) and "CONV" not in run.final_holdings
    assert led["principal"][at(converted)] == pytest.approx(40_000.0 * 30.0 - 1e6 * (1.0 + bonds["CONV"].accrued(converted)), rel=1e-9)
    frn = bonds["FRN"].cashflows({"TERM": {D("2024-11-28"): -0.005}}, forecast=fi.Curve.flat(D("2024-11-28"), -0.004)).filter(pl.col("kind") == "coupon")
    assert frn["amount"][0] == 0.0, "floored: −0.5% + 0.2% pays nothing"
    assert run.max_identity_error < 1e-9


def test_p08_an_inflation_coupon_on_the_index_as_booked_or_as_revised():
    """P8: a linker's coupon on its index three months back, published and then revised: the
    accounts book the first release unless the run restates, and decisions see each from its
    publication."""
    linker = fi.Bond("2024-01-15", "2034-01-15", fi.Coupon.inflation("CPI", real=0.01, base=300.0, lag=3, interpolate=False), day_count="ACT/ACT ICMA", frequency=2)
    dates = weekdays(D("2025-06-02"), 50)
    marks = pl.DataFrame({"isin": "IL", "date": dates, "clean": 100.0})
    months = [D(f"2024-{m:02d}-01") for m in range(1, 13)] + [D(f"2025-{m:02d}-01") for m in range(1, 7)]
    rows = [(m, 300.0 + i, dt.datetime(m.year + (m.month == 12), m.month % 12 + 1, 15)) for i, m in enumerate(months)]
    rows.append((D("2025-04-01"), 318.0, dt.datetime(2025, 6, 20)))  # April revised from 315 to 318
    fixings = pl.DataFrame(rows, schema=["date", "rate", "published"], orient="row").with_columns(index=pl.lit("CPI"))
    coupons = {}
    for outcomes in ("as_booked", "restated"):
        _, _, run = run_debt({"IL": linker}, marks, fixings=fixings, outcomes=outcomes)
        led = run.ledger()
        k = led["time"].dt.date().to_list().index(D("2025-07-15"))
        coupons[outcomes] = led["cash"][k] - led["cash"][k - 1]
    assert coupons["as_booked"] == pytest.approx(1e6 * 0.005 * 315.0 / 300.0, rel=1e-9)
    assert coupons["restated"] == pytest.approx(1e6 * 0.005 * 318.0 / 300.0, rel=1e-9)


def test_r05_a_cds_index_constituent_default_settles_and_the_index_carries_on_smaller():
    """R5: protection bought on an index of four names, 1% a year quarterly on ACT/360. A name
    defaults with a recovery of 40: the buyer is paid 60% of its quarter of the notional, less the
    premium that quarter accrued; the index carries on at three quarters, and its next premium is
    on what remains."""
    dates = weekdays(D("2025-03-20"), 90)
    default = D("2025-05-06")
    ws = fi.Workspace()
    ws.series("cdx", pl.DataFrame({"index": "IDX", "date": dates, "price": 99.0}), entity="index", kind="cds", time="date", values={"price": "price"})
    ws.series("credit", pl.DataFrame({"name": ["B"], "date": [default], "default": [1.0], "recovery": [40.0]}), entity="name", kind="issuer", time="date",
              values={"default": "number", "recovery": "price"})
    cds = ws.cds("IDX", "cdx.price", coupon=0.01, start="2025-03-20", maturity="2030-06-20", names={n: 0.25 for n in "ABCD"}, events="credit")
    rec = ws.records(cds, fi.Calendar(dates))
    buy = fi.Decision.python(lambda ctx: fi.quantities({"IDX": 1e7}) if ctx.k == 0 else None)
    run = ws.run(fi.Strategy(buy, schedule="all", execution_lag=0, fill=fi.Fill.research(0.0)), rec)
    led = run.ledger()
    at = led["time"].dt.date().to_list().index
    accrued = 0.01 * (default - D("2025-03-20")).days / 360
    assert led["cash"][at(default)] - led["cash"][at(default) - 1] == pytest.approx(1e7 * 0.25 * (0.60 - accrued), rel=1e-12)
    coupon = D("2025-06-20")
    assert led["cash"][at(coupon)] - led["cash"][at(coupon) - 1] == pytest.approx(-1e7 * 0.75 * 0.01 * 92 / 360, rel=1e-12), "on what remains"
    face = rec.frame().filter(pl.col("entity") == "IDX")
    assert led["other"][at(default)] == pytest.approx(1e7 * 0.25 * (0.60 - 0.01), rel=1e-9), "protection against the mark it settles"
    assert run.max_identity_error < 1e-9 and face.height == len(dates)
    sold = ws.run(fi.Strategy(fi.Decision.python(lambda ctx: fi.quantities({"IDX": -1e7}) if ctx.k == 0 else None), schedule="all", execution_lag=0), rec)
    assert sold.final_nav - 1e8 == pytest.approx(-(run.final_nav - 1e8), rel=1e-9), "the seller's account is the buyer's, reversed"
