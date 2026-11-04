"""Deals from Python (spec, change 3): a collateral book, its priority of payments, and tranches
that other books hold. The Rust tests (crates/fi-research/tests/structured.rs) hold the full
fixtures; these run the same cases through the user's API."""

from __future__ import annotations

import datetime as dt

import polars as pl
import pytest

import fi_engine as fi

START = dt.date(2025, 1, 15)


def quarters(n: int) -> list[dt.date]:
    """Payment dates 90 days apart: a quarter is exactly 0.25 of a year, Act/360."""
    return [START + dt.timedelta(days=90 * k) for k in range(n)]


def loans(ws: fi.Workspace, dates: list[dt.date], spec: dict) -> fi.Contracts:
    """`spec`: name -> (clean price, {date index: (interest, principal, written down, written up)}),
    per unit of original face."""
    names = list(spec)
    ws.series(
        "loans",
        pl.DataFrame({
            "loan": [n for _ in dates for n in names],
            "date": [d for d in dates for _ in names],
            "clean": [float(spec[n][0]) for _ in dates for n in names],
            "accrued": [0.0] * (len(dates) * len(names)),
        }),
        entity="loan", kind="loan", time="date", values={"clean": "price", "accrued": "price"}, grid=True,
    )
    cols = ["interest", "principal_face", "writedown_face", "writeup_face"]
    rows = [(n, dates[k], *map(float, f)) for n, (_, flows) in spec.items() for k, f in flows.items()]
    schema = {"loan": pl.String, "date": pl.Date} | dict.fromkeys(cols, pl.Float64)
    ws.series("loan_flows", pl.DataFrame(rows, schema=schema, orient="row"), entity="loan", kind="loan", time="date",
              values=dict.fromkeys(cols, "number"))
    return ws.supplied_bonds("loans", flows="loan_flows")


def at(ws: fi.Workspace, series: str, entity: str, date: dt.date, column: str) -> float | None:
    f = ws.frame(series).filter((pl.col("entity") == entity) & (pl.col("time").dt.date() == date))
    return None if f.is_empty() else f[column][0]


def paid(ws: fi.Workspace, deal: str, tranche: str, date: dt.date, column: str = "interest") -> float:
    """A tranche's flow per unit of original face on a payment date (zero without a row)."""
    v = at(ws, f"{deal}.flows", f"{deal}.{tranche}", date, column)
    return 0.0 if v is None else v


def clo(par: float, rounding=None):
    """F01's control: a numerator of `par`, debt of 100, a threshold of 1.03, cured from interest."""
    ws = fi.Workspace()
    dates = quarters(2)
    cal = fi.Calendar(dates)
    pool = loans(ws, dates, {"L": (100, {1: (5 / par, 0, 0, 0)})})
    deal = ws.deal(
        "clo", cal, collateral=pool, opening={"L": par},
        tranches=[fi.Tranche("A", 100.0, coupon=0.0), fi.Tranche.equity("E", 10.0)],
        tests=[fi.Test.oc("A_OC", ["A"], 1.03, cure=["A"])],
        interest=["interest:A", "test:A_OC", "residual:E"],
        payments=[dates[1]],
        rounding=rounding,
    )
    assert deal.writes == [f"clo.{s}" for s in ["flows", "marks", "distributions", "tests", "accounts", "collateral", "claims"]]
    ws.evaluate([deal], cal)
    return ws, dates


def test_f01_the_cure_is_debt_less_numerator_over_threshold():
    ws, dates = clo(102.0)
    t = dates[1]
    cure = 100 - 102 / 1.03
    assert at(ws, "clo.tests", "clo.A_OC", t, "cure") == pytest.approx(cure, rel=1e-12)
    assert at(ws, "clo.tests", "clo.A_OC", t, "after") == pytest.approx(1.03, rel=1e-12)
    assert paid(ws, "clo", "A", t, "principal_face") * 100 == pytest.approx(cure, rel=1e-12)
    assert paid(ws, "clo", "E", t) * 10 == pytest.approx(5 - cure, rel=1e-12)
    ws, dates = clo(103.0)
    assert at(ws, "clo.tests", "clo.A_OC", dates[1], "passed") == 1.0
    assert at(ws, "clo.tests", "clo.A_OC", dates[1], "cure") == 0.0


def test_f01_in_whole_cents_the_cure_is_the_smallest_whole_cent_that_cures():
    # F01 is stated before contractual rounding. In cents, the largest debt that passes is
    # 99.02 (102 / 1.03 = 99.029…), so 0.98 is diverted, not 0.970873….
    ws, dates = clo(102.0, rounding=("USD", "half_up"))
    t = dates[1]
    cure = at(ws, "clo.tests", "clo.A_OC", t, "cure")
    assert cure == pytest.approx(0.98)
    assert at(ws, "clo.tests", "clo.A_OC", t, "passed_after") == 1.0
    assert paid(ws, "clo", "A", t, "principal_face") * 100 == pytest.approx(0.98)
    assert paid(ws, "clo", "E", t) * 10 == pytest.approx(4.02)
    assert 102 / (100 - (cure - 0.01)) < 1.03, "one cent less does not cure"


def f02():
    ws = fi.Workspace()
    dates = quarters(4)
    cal = fi.Calendar(dates)
    pool = loans(ws, dates, {"L": (100, {1: (0.025, 0, 0, 0), 2: (0.016, 0, 0, 0), 3: (0.035, 0, 0, 0)})})
    deal = ws.deal(
        "clo", cal, collateral=pool, opening={"L": 100.0},
        tranches=[fi.Tranche("A", 70.0, coupon=0.08), fi.Tranche("B", 20.0, coupon=0.12, shortfall="defer_compound"),
                  fi.Tranche.equity("E", 10.0)],
        fees=[fi.Fee("senior", fixed=0.1)],
        tests=[fi.Test.ic("A_IC", ["A"], 1.2, cure=["A"])],
        interest=["fee:senior", "interest:A", "test:A_IC", "interest:B", "residual:E"],
        principal=["sequential:A,B"],
        payments=dates[1:],
    )
    ws.evaluate([deal], cal)
    return ws, dates


def test_f02_junior_interest_is_deferred_with_interest_and_paid_later_in_priority():
    ws, d = f02()
    assert paid(ws, "clo", "B", d[2]) == 0.0
    assert paid(ws, "clo", "E", d[2]) == 0.0, "the equity receives nothing allocated to senior protection"
    assert at(ws, "clo.marks", "clo.B", d[2], "deferred") == pytest.approx(0.6)
    assert paid(ws, "clo", "B", d[3]) * 20 == pytest.approx(0.6 + 0.618)
    dist = ws.frame("clo.distributions").with_columns(total=pl.col("interest") + pl.col("principal") + pl.col("fee"))
    by_date = dist.group_by(pl.col("time").dt.date()).agg(pl.col("total").sum()).sort("time")
    assert by_date["total"].to_list() == pytest.approx([2.5, 1.6, 3.5]), "distributions reconcile to the cash collected"


def test_f06_principal_cash_is_15_not_17():
    ws = fi.Workspace()
    dates = quarters(2)
    cal = fi.Calendar(dates)
    pool = loans(ws, dates, {"L": (100, {1: (0, 0.15, 0.02, 0)})})
    deal = ws.deal(
        "abs", cal, collateral=pool, opening={"L": 100.0},
        tranches=[fi.Tranche("A", 80.0, coupon=0.0), fi.Tranche("B", 20.0, coupon=0.0)],
        losses=fi.write_down(["B", "A"]),
        interest=[], principal=["sequential:A,B"], payments=[dates[1]],
    )
    ws.evaluate([deal], cal)
    t = dates[1]
    assert at(ws, "abs.collateral", "abs", t, "principal") == pytest.approx(15.0)
    assert at(ws, "abs.distributions", "abs.A", t, "principal") == pytest.approx(15.0)
    assert paid(ws, "abs", "B", t, "writedown_face") * 20 == pytest.approx(2.0)
    balances = at(ws, "abs.marks", "abs.A", t, "balance") + at(ws, "abs.marks", "abs.B", t, "balance")
    assert balances == pytest.approx(83.0)


def test_f05_r12_a_python_manager_stops_buying_when_the_trigger_ends_reinvestment():
    ws = fi.Workspace()
    dates = quarters(7)
    cal = fi.Calendar(dates)
    pool = loans(ws, dates, {"R": (100, {k: (0.015, 0.2, 0, 0) for k in range(1, 6)}), "N": (100, {})})
    ws.series("perf", pl.DataFrame({"date": dates[1:], "excess": [2.0, 1.0, -3.0, -1.0, 5.0, 5.0]}), time="date", values={"excess": "number"})
    bought = []

    def replenish(ctx):
        cash = ctx.cash["base"]
        if ctx.state("reinvesting") == 1.0 and cash > 1e-9:
            bought.append(ctx.date)
            return fi.orders({"N": cash / 1.0})
        return None

    deal = ws.deal(
        "abs", cal, collateral=pool, opening={"R": 100.0},
        tranches=[fi.Tranche("A", 80.0, coupon=0.0), fi.Tranche.equity("E", 20.0)],
        triggers=[fi.Trigger("early_amortization", "perf.excess", below=0.0, lookback=3, ends_reinvestment=True)],
        interest=["residual:E"], principal=["sequential:A when amortising"],
        payments=dates[1:], reinvest_until=dates[-1] + dt.timedelta(days=365),
        manager=fi.Policy(fi.Decision.python(replenish), schedule="all", lag=0),
    )
    ws.evaluate([deal], cal)
    assert at(ws, "abs.tests", "abs.early_amortization", dates[3], "passed") == 1.0, "exactly at the boundary"
    assert at(ws, "abs.tests", "abs.early_amortization", dates[4], "passed") == 0.0
    assert bought == [d for d in dates[1:5]], "purchases stop from the decision after the trigger"
    assert paid(ws, "abs", "A", dates[5], "principal_face") * 80 == pytest.approx(20.0)


def test_tranches_are_bonds_another_book_holds():
    ws, d = f02()
    rec = ws.records(ws.tranches("clo", ["A", "E"]), fi.Calendar(d))
    run = ws.run(fi.Book([], opening=[("clo.A", 10.0), ("clo.E", 5.0)], initial_cash=0.0), rec)
    led = run.ledger()
    assert run.max_identity_error < 1e-9
    flows = ws.frame("clo.flows")
    per_unit = {e: flows.filter(pl.col("entity") == e)["interest"].sum() for e in ["clo.A", "clo.E"]}
    assert led["income"].sum() == pytest.approx(10 * per_unit["clo.A"] + 5 * per_unit["clo.E"])
    assert led["nav"][0] == pytest.approx(15.0), "debt at par, equity at book value"


def test_rights_are_checked_against_the_terms():
    ws = fi.Workspace()
    dates = quarters(4)
    cal = fi.Calendar(dates)
    pool = loans(ws, dates, {"L": (100, {k: (0.02, 0, 0, 0) for k in range(1, 4)})})
    terms = {
        "collateral": pool, "opening": {"L": 100.0}, "payments": dates[1:],
        "tranches": [fi.Tranche("A", 80.0, coupon=0.05), fi.Tranche.equity("E", 20.0), fi.Tranche.replacement("A-R", 80.0, 0.04)],
        "interest": ["interest:A", "residual:E"],
        "rights": [fi.Right("refinance", "E", majority=0.5, non_call=dates[2])],
    }
    early = fi.Exercise("refinance", dates[1], "E", 0.6, redeem=["A"], replacements=["A-R"])
    with pytest.raises(fi.InvalidError, match="non-call"):
        ws.deal("clo", cal, exercises=[early], **terms)
    on_time = fi.Exercise("refinance", dates[2], "E", 0.6, redeem=["A"], replacements=["A-R"])
    ws.evaluate([ws.deal("clo", cal, exercises=[on_time], **terms)], cal)
    claims = ws.frame("clo.claims")
    assert claims["entity"].to_list() == ["clo.A-R"] and claims["replaces"].to_list() == ["clo.A"]
    assert paid(ws, "clo", "A", dates[2], "principal_face") == pytest.approx(1.0)
    assert paid(ws, "clo", "A-R", dates[3]) * 80 == pytest.approx(80 * 0.04 / 4)


def test_a_misspelt_step_names_the_actions():
    ws = fi.Workspace()
    dates = quarters(2)
    pool = loans(ws, dates, {"L": (100, {})})
    with pytest.raises(ValueError, match="unknown action pay"):
        ws.deal("clo", fi.Calendar(dates), collateral=pool, opening={"L": 1.0}, tranches=[fi.Tranche("A", 1.0)], interest=["pay:A"])
    with pytest.raises(ValueError, match="no tranche Z"):
        ws.deal("clo", fi.Calendar(dates), collateral=pool, opening={"L": 1.0}, tranches=[fi.Tranche("A", 1.0)], interest=["interest:Z"])


# --------------------------------------------------------------- one producer per state (change 4)

def counterfactual_world():
    """A CLO whose manager trades L2 for L3 at the third date (recorded), or for L4, which then
    defaults, in the alternative."""
    ws = fi.Workspace()
    dates = quarters(5)
    cal = fi.Calendar(dates)
    paying = {k: (0.02, 0, 0, 0) for k in range(1, 5)}
    pool = loans(ws, dates, {"L1": (100, paying), "L2": (100, paying), "L3": (100, paying),
                             "L4": (100, {1: (0.02, 0, 0, 0), 2: (0.02, 0, 0, 0), 3: (0, 0, 0.5, 0)})})
    for name, buy in [("trades", "L3"), ("alt", "L4")]:
        ws.series(name, pl.DataFrame({"loan": ["L2", buy], "date": [dates[2]] * 2, "qty": [-50.0, 50.0]}), entity="loan", kind="loan", time="date",
                  values={"qty": "number"})
    ws.series("trustee", pl.DataFrame({"tranche": ["cf.A"], "date": [dates[1]], "interest": [0.01], "principal_face": [0.0]}), entity="tranche",
              kind="tranche", time="date", values={"interest": "number", "principal_face": "number"}, describes="cf.flows")

    def clo(name, manager):
        return ws.deal(
            name, cal, collateral=pool, opening={"L1": 50.0, "L2": 50.0},
            tranches=[fi.Tranche("A", 80.0, coupon=0.04), fi.Tranche.equity("E", 20.0)],
            tests=[fi.Test.oc("A_OC", ["A"], 1.1, cure=["A"])],
            interest=["interest:A", "test:A_OC", "residual:E"], payments=dates[1:],
            manager=fi.Policy(manager, schedule="all", lag=0), reads=["trades", "alt"],
        )

    return ws, cal, dates, clo


def test_f03_a_counterfactual_refuses_the_real_deals_reported_flows():
    ws, cal, dates, clo = counterfactual_world()
    branch = fi.Decision.branch(dates[2], fi.Decision.trades("trades.qty"), fi.Decision.trades("alt.qty"))
    with pytest.raises(fi.InvalidError, match="counterfactual"):
        ws.evaluate([clo("cf", branch)], cal, substitute={"cf.flows": "trustee"})
    report = ws.evaluate([clo("cf", branch)], cal)
    assert report["mode"] == "counterfactual"
    assert ws.provenance("cf.flows")["intervention"][0] == "counterfactual"
    # The baseline replays the recorded trades: an observation may stand in for its flows.
    report = ws.evaluate([clo("base", fi.Decision.trades("trades.qty"))], cal, substitute={"base.flows": "trustee"})
    assert report["mode"] == "replay" and report["substituted"] == {"base.flows": "trustee"}
    assert ws.provenance("base.flows")["producer"] == "observation"


def test_a_runs_mode_follows_its_decisions_and_its_world():
    ws, cal, dates, clo = counterfactual_world()
    branch = fi.Decision.branch(dates[2], fi.Decision.trades("trades.qty"), fi.Decision.trades("alt.qty"))
    ws.evaluate([clo("cf", branch), clo("base", fi.Decision.trades("trades.qty"))], cal)
    held = fi.Book([], opening=[("base.A", 10.0)], initial_cash=0.0)
    assert ws.run(held, ws.records(ws.tranches("base", ["A"]), cal)).mode == "holdings replay"
    assert ws.run(fi.Book([], opening=[("cf.A", 10.0)], initial_cash=0.0), ws.records(ws.tranches("cf", ["A"]), cal)).mode == "counterfactual"
    rec = ws.records(ws.tranches("base", ["A"]), cal)
    computed = fi.Strategy(fi.Decision.python(lambda ctx: {"base.A": 0.5}), initial_cash=100.0, execution_lag=0)
    assert ws.run(computed, rec).mode == "policy backtest"
    ws.series("shocked", pl.DataFrame({"date": dates, "rate": [0.05] * 5}), time="date", values={"rate": "number"}, scenario="rates +100bp")
    assert ws.provenance("shocked")["intervention"] == ("scenario", "rates +100bp")


def test_p2_e4_a_holdings_replay_reconciles_with_its_differences_visible():
    ws, d = f02()
    rec = ws.records(ws.tranches("clo", ["A"]), fi.Calendar(d))
    run = ws.run(fi.Book([], opening=[("clo.A", 10.0)], initial_cash=0.0), rec)
    nav = run.ledger()["nav"].to_list()
    official = nav[:-1] + [nav[-1] - 0.25]
    ws.series("admin", pl.DataFrame({"date": d, "nav": official}), time="date", values={"nav": "number"})
    rec_ = ws.reconcile(run, "admin.nav")
    assert rec_["difference"].to_list()[:-1] == pytest.approx([0.0] * 3)
    assert rec_["difference"][-1] == pytest.approx(0.25)


# ------------------------------------------------------------------------------- worlds (change 6)

def test_f25_waterfalls_run_in_each_world_before_the_average():
    """A tranche attaching at 10 and detaching at 20, pool losses of 0 or 20: the expected tranche
    loss is 5. The waterfall on the mean pool loss gives 0, which is not equivalent."""
    base = fi.Workspace()
    dates = quarters(3)
    cal = fi.Calendar(dates)
    base.series("loans", pl.DataFrame({"loan": ["L"] * 3, "date": dates, "clean": [100.0] * 3, "accrued": [0.0] * 3}), entity="loan", kind="loan",
                time="date", values={"clean": "price", "accrued": "price"}, grid=True)

    def tranche_loss(pool_loss: float) -> float:
        ws = base.world()
        ws.series("pool_flows", pl.DataFrame({"loan": ["L"], "date": [dates[2]], "writedown_face": [pool_loss / 100]}), entity="loan", kind="loan",
                  time="date", values={"writedown_face": "number"}, scenario=f"pool loss {pool_loss}")
        pool = ws.supplied_bonds("loans", flows="pool_flows")
        deal = ws.deal("d", cal, collateral=pool, opening={"L": 100.0},
                       tranches=[fi.Tranche("S", 80.0), fi.Tranche("M", 10.0), fi.Tranche("E", 10.0)],
                       losses=fi.write_down(["E", "M", "S"]), interest=[], payments=dates[1:])
        assert ws.evaluate([deal], cal)["mode"] == "scenario"
        return ws.frame("d.flows").filter(pl.col("entity") == "d.M")["writedown_face"].sum() * 10

    assert (tranche_loss(0.0) + tranche_loss(20.0)) / 2 == pytest.approx(5.0)
    assert tranche_loss(10.0) == pytest.approx(0.0)


def test_worlds_from_python_share_their_draws_and_reproduce():
    base = fi.Workspace()
    dates = [START + dt.timedelta(days=30 * k) for k in range(12)]
    cal = fi.Calendar(dates)

    def last(world: int) -> float:
        ws = base.world()
        report = ws.evaluate([ws.gaussian_paths("rate", 0.04, mean=0.04, reversion=0.5, vol=0.01, seed=3, world=world)], cal)
        assert report["mode"] == "simulation" and ws.provenance("rate")["intervention"] == ("simulation", "rate")
        return ws.frame("rate")["value"][-1]

    finals = [last(w) for w in range(16)]
    assert finals == [last(w) for w in range(16)]
    assert len(set(finals)) == 16
    ws = base.world()
    loans = [("LX", "X", 0.06, dates[-1], 0.5, 0.4), ("BX", "X", 0.05, dates[-1], 0.5, 0.6)]
    ws.evaluate([ws.defaults("flows", loans, dates[1:], correlation=0.3, seed=1, world=7)], cal)
    f = ws.frame("flows").filter(pl.col("defaulted") == 1.0)
    if not f.is_empty():
        assert f["time"].n_unique() == 1, "one borrower defaults once, in every facility"
        assert sorted(f["principal_face"].to_list()) == pytest.approx([0.4, 0.6])
