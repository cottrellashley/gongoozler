"""Books from Python (spec changes 1, 2 and 5): currencies, settlement, the cash and short guards,
capital flows, the horizon, orders that work over time, policies, constraints, books that share
execution, and the journal."""

from __future__ import annotations

import datetime as dt

import numpy as np
import polars as pl
import pytest

import fi_engine as fi
from conftest import weekdays


def world(days: int = 60, seed: int = 1, flat_b: bool = False):
    """Two levels, A in USD and B in EUR, an FX rate for EUR, and a 5% rate."""
    rng = np.random.default_rng(seed)
    dates = weekdays(dt.date(2025, 1, 6), days)
    walk = lambda start, vol: start * np.exp(np.cumsum(np.r_[0.0, rng.normal(0, vol, days - 1)]))  # noqa: E731
    a, b = walk(100.0, 0.01), (np.full(days, 100.0) if flat_b else walk(100.0, 0.01))
    ws = fi.Workspace()
    ws.currency("USD")
    ws.series("lv", pl.DataFrame({"index": ["A", "B"] * days, "date": [d for d in dates for _ in range(2)],
                                  "level": np.column_stack([a, b]).ravel()}), entity="index", time="date", values={"level": "number"}, grid=True)
    ws.series("fx", pl.DataFrame({"ccy": ["EUR"] * days, "date": dates, "rate": walk(1.1, 0.005)}), entity="ccy", kind="currency", time="date",
              values={"rate": "number"}, grid=True)
    ws.series("sofr", pl.DataFrame({"date": dates, "rate": [5.0] * days}), time="date", values={"rate": "%"})
    rec = ws.records(ws.total_return("A", "lv.level") + ws.total_return("B", "lv.level", currency="EUR"), ws.calendar("lv"))
    return ws, rec, dates


USD = {"currency": "USD", "fx": "fx.rate"}


def once(targets, k: int = 0):
    return fi.Decision.python(lambda ctx: targets if ctx.k == k else None, name="once")


def terms(led: pl.DataFrame) -> float:
    return float((led["income"] + led["price"] + led["principal"] + led["other"] - led["financing"] + led["currency"] - led["costs"]
                  + led["capital"]).sum())


def test_a_book_in_two_currencies_reconciles():
    ws, rec, _ = world()
    book = fi.Strategy(fi.Decision.python(lambda ctx: {"A": 0.5, "B": 0.5}), fill=fi.Fill.research(2.0), currency="USD", fx="fx.rate",
                       settlement=2)
    run = ws.run(book, rec)
    led = run.ledger()
    assert run.max_identity_error < 1e-9
    assert led["nav"][-1] - 1e8 == pytest.approx(terms(led), rel=1e-9)
    assert led["currency"].abs().sum() > 0, "the EUR position translates"
    assert led["cash"].min() >= -1e-6, "cash never below zero"
    assert (led["unsettled"] != 0).any(), "trades settle two steps later"


def test_a_foreign_position_with_flat_local_prices_earns_only_currency():
    ws, rec, _ = world(flat_b=True)
    run = ws.run(fi.Strategy(once(fi.quantities({"B": 1000.0})), schedule="all", execution_lag=0, **USD), rec)
    led = run.ledger()
    assert abs(led["price"].sum()) < 1e-6
    assert led["nav"][-1] - led["nav"][0] == pytest.approx(led["currency"].sum(), abs=1e-6)


def test_r1_a_coupon_on_an_unsettled_purchase_reaches_its_owner():
    """R1: the buyer is entitled to a coupon paid between trade and settlement; the seller, still
    holder of record, owes it until settlement. Accrued interest is not counted twice."""
    dates = weekdays(dt.date(2026, 1, 5), 12)
    accrued = [0.5 * k if k < 5 else 0.5 * (k - 5) for k in range(12)]

    def run(settle: int) -> fi.Run:
        ws = fi.Workspace()
        ws.series("marks", pl.DataFrame({"isin": ["B"] * 12, "date": dates, "clean": [100.0] * 12, "accrued": accrued}), entity="isin",
                  kind="bond", time="date", values={"clean": "price", "accrued": "price"}, grid=True)
        ws.series("flows", pl.DataFrame({"isin": ["B"], "date": [dates[5]], "interest": [0.025]}), entity="isin", kind="bond", time="date",
                  values={"interest": "number"})
        rec = ws.records(ws.supplied_bonds("marks", flows="flows"), ws.calendar("marks"))
        return ws.run(fi.Strategy(once(fi.quantities({"B": 1e6}), k=4), schedule="all", execution_lag=0, settlement=settle), rec)

    now, later = run(0), run(3)
    assert now.ledger()["income"].sum() == pytest.approx(later.ledger()["income"].sum(), rel=1e-12)
    assert later.ledger()["unsettled"][5] == pytest.approx(-1e6 * 1.02 + 1e6 * 0.025)
    assert later.ledger()["cash"][7] == pytest.approx(now.ledger()["cash"][7])


def test_the_cash_guard_borrowing_and_unlimited_cash():
    ws, rec, _ = world()
    ask = once({"A": 1.2})
    guarded = ws.run(fi.Strategy(ask, schedule="all", execution_lag=0, **USD), rec)
    assert guarded.cash_limited == 1 and guarded.ledger()["cash"].min() >= -1e-6
    borrowed = ws.run(fi.Strategy(once({"A": 1.1}), schedule="all", execution_lag=0, cash="borrow", borrow_limit=1.5e7, borrow_rate="sofr.rate", **USD), rec)
    assert borrowed.cash_limited == 0 and borrowed.ledger()["financing"].sum() > 0, "borrowing is charged"
    free = ws.run(fi.Strategy(ask, schedule="all", execution_lag=0, cash="unlimited", **USD), rec)
    assert free.ledger()["cash"].min() < -1e7, "unlimited borrowing is an explicit choice"
    with pytest.raises(fi.InvalidError, match="can be spent"):
        ws.run(fi.Strategy(ask, schedule="all", execution_lag=0, cash="forbid_fail", **USD), rec)


def test_shorts_need_a_borrow():
    ws, rec, _ = world()
    short = once(fi.quantities({"A": -1000.0}))
    cut = ws.run(fi.Strategy(short, schedule="all", execution_lag=0, **USD), rec)
    assert cut.final_holdings == {} and cut.short_limited == 1
    lent = ws.run(fi.Strategy(short, schedule="all", execution_lag=0, shorts="borrow", borrow_fee_bp=40.0, **USD), rec)
    assert lent.final_holdings == {"A": -1000.0} and lent.ledger()["financing"].sum() > 0


def test_capital_flows_by_series_and_by_model():
    ws, rec, dates = world()
    ws.series("subs", pl.DataFrame({"date": [dates[10], dates[20]], "amount": [1e7, -2e7]}), time="date", values={"amount": "number"})
    run = ws.run(fi.Strategy(once({"A": 0.5}), schedule="all", execution_lag=0, flows=fi.flows.series("subs.amount"), **USD), rec)
    led = run.ledger()
    assert led["capital"].sum() == pytest.approx(-1e7)
    for k in (10, 20):
        r = led["nav_per_unit"][k] / led["nav_per_unit"][k - 1]
        assert r == pytest.approx((led["nav"][k] - led["capital"][k]) / led["nav"][k - 1], rel=1e-12), "flows buy units, not returns"

    def redeem_after_losses(ctx):
        """Redemptions that follow performance: 5% of NAV after a falling week."""
        npu = ctx["nav_per_unit"]
        return -0.05 * ctx["nav"][-1] if len(npu) > 5 and ctx["k"] % 5 == 0 and npu[-1] < npu[-6] else 0.0

    modelled = ws.run(fi.Strategy(once({"A": 0.5}), schedule="all", execution_lag=0, flows=fi.flows.model(redeem_after_losses), **USD), rec)
    assert modelled.ledger()["capital"].min() < 0 and modelled.max_identity_error < 1e-9


def test_r7_a_redemption_due_before_sale_proceeds_settle_is_a_failure():
    """R7: a redemption payable falls due before the sale that funds it settles. The cash
    shortage is detected and the book fails economically; the failed path is still a result."""
    ws, rec, dates = world(days=40)
    ws.series("subs", pl.DataFrame({"date": [dates[20]], "amount": [-2e7]}), time="date", values={"amount": "number"})

    @fi.decision
    def invest_then_raise(ctx):
        return {"A": 0.999} if ctx.k == 0 else ({"A": 0.75} if ctx.k == 20 else None)

    run = ws.run(fi.Strategy(invest_then_raise, schedule="all", execution_lag=0, settlement=3, flows=fi.flows.series("subs.amount", settle=1), **USD), rec)
    assert run.failed_at == dates[21] and run.shortfall > 1e7
    assert run.ledger()["nav_per_unit"].len() == 40, "a failed path stays in the results"


def test_the_horizon_liquidates_through_execution():
    ws, rec, _ = world(days=30)
    run = ws.run(fi.Strategy(once(fi.quantities({"A": 1000.0})), schedule="all", execution_lag=0, fill=fi.Fill.research(10.0), horizon="liquidate", **USD), rec)
    value = run.ledger()["nav"][-1] - run.ledger()["cash"][-1]
    assert run.liquidation_nav == pytest.approx(run.final_nav - value * 10e-4, rel=1e-12)
    silent = fi.Execution.rfq(response=0.0)
    unsold = ws.run(fi.Book([fi.Policy(once(fi.quantities({"A": 1000.0})), schedule="all", lag=0)], execution=silent, horizon="liquidate", **USD), rec)
    assert unsold.trades == 0 and unsold.order_stats["expired"] == 1


def test_r6_p10_orders_persist_until_filled_or_expired():
    """R6 and P10: a request nobody answers keeps working until it expires; size caps fill part
    per attempt; later decisions see the working order; only executed quantities change the
    book."""
    ws, rec, _ = world(days=40)
    seen = []

    def decide(ctx):
        seen.append(len(ctx.orders))
        return fi.quantities({"A": 1000.0}) if ctx.k == 1 else None

    rfq = fi.Execution.rfq(dealers=2, response=0.35, half_spread_bp=2.0, max_size=300.0, seed=4, latency=1)
    book = fi.Book([fi.Policy(fi.Decision.python(decide), schedule="all", lag=0, time_in_force=6)], execution=rfq, journal=True, **USD)
    run = ws.run(book, rec)
    orders = run.orders()
    assert orders.height == 1
    o = orders.row(0, named=True)
    assert 1 <= o["attempts"] <= 6 and o["status"] in ("filled", "partial", "expired")
    assert o["filled"] == pytest.approx(run.final_holdings.get("A", 0.0))
    assert any(n == 1 for n in seen[2:6]) or o["status"] == "filled", "a later decision sees the working order"
    journal = run.journal()
    assert journal.filter(pl.col("kind") == "fill").height == run.trades
    assert journal.filter(pl.col("kind") == "fill")["cause"].str.starts_with("order").all(), "every fill names its order"


def test_policies_have_scopes_priorities_and_notes():
    ws, rec, _ = world(days=30)

    @fi.decision(name="main")
    def main(ctx):
        ctx.note("main: rebalanced")
        return {"A": 0.4}

    hedge = fi.Decision.python(lambda ctx: fi.quantities({"A": 7.0, "B": 1000.0}), name="hedge")
    book = fi.Book([fi.Policy(main, schedule=[rec.calendar.times()[0]], scope=["A"], priority=1, lag=0),
                    fi.Policy(hedge, schedule=[rec.calendar.times()[0]], lag=0)], currency="USD", fx="fx.rate", journal=True)
    run = ws.run(book, rec)
    assert run.final_holdings["B"] == 1000.0 and run.final_holdings["A"] != 7.0, "the higher priority keeps A"
    notes = run.journal().filter(pl.col("kind") == "note")["note"].to_list()
    assert "main: rebalanced" in notes and any("higher-priority" in n for n in notes)


def test_constraints_stay_what_they_are_declared():
    with pytest.raises(fi.InvalidError, match="mandatory"):
        fi.Constraint.soft("cap", penalty=10.0, max=0.1, mandatory=True)
    with pytest.raises(fi.InvalidError, match="positive penalty"):
        fi.Constraint.soft("cap", penalty=0.0, max=0.1)
    ws, rec, _ = world(days=30)
    cap = fi.Constraint.hard("A at most 30%", contracts=["A"], max=0.3, relative=True, mandatory=True)
    run = ws.run(fi.Strategy(once({"A": 0.5}), schedule="all", execution_lag=0, constraints=[cap], **USD), rec)
    b = run.breaches[0]
    assert b["constraint"] == "A at most 30%" and b["count"] == 30 and b["worst"] > 0.19
    strict = fi.Constraint.hard("A at most 30%", contracts=["A"], max=0.3, relative=True, fail=True)
    with pytest.raises(fi.InvalidError, match="breached"):
        ws.run(fi.Strategy(once({"A": 0.5}), schedule="all", execution_lag=0, constraints=[strict], **USD), rec)


def test_p11_competing_accounts_share_capacity():
    """P11: two accounts that must be implementable together share a dealer's capacity, allocated
    pro rata."""
    ws, rec, _ = world(days=20)
    value = rec.frame().filter(pl.col("entity") == "A")["value"][0]
    books = [fi.Strategy(once(fi.quantities({"A": 10_000.0})), schedule="all", execution_lag=0, **USD) for _ in range(2)]
    shared = fi.Execution.capacity(fi.Execution.fill(fi.Fill.research()), 10_000.0 * value)
    runs = ws.run_together(books, rec, shared)
    assert [r.final_holdings["A"] for r in runs] == [pytest.approx(5_000.0)] * 2
    assert all(r.unfilled_orders == 1 for r in runs)


def test_a_portfolio_trade_prices_the_basket():
    ws, rec, _ = world(days=20)
    pt = fi.Execution.portfolio(half_spread_bp=2.0, impact_bp=1.0, acceptance=1.0)
    run = ws.run(fi.Book([fi.Policy(once(fi.quantities({"A": 20_000.0})), schedule="all", lag=0)], execution=pt, **USD), rec)
    v = rec.frame().filter(pl.col("entity") == "A")["value"][0]
    notional = 20_000.0 * v
    assert run.ledger()["costs"][0] == pytest.approx(notional * (2.0 + np.sqrt(notional / 1e6)) * 1e-4, rel=1e-9)


def test_a_declared_step_order_moves_events_that_share_a_time_and_both_orders_reconcile():
    """Review H3: events at one time resolve in the book's declared order. A decision investing the
    cash it can spend invests a subscription at once when capital flows run first (the default),
    and a step later when they run after execution."""
    ws, rec, dates = world(days=30)
    ws.series("subs", pl.DataFrame({"date": [dates[10]], "amount": [1e7]}), time="date", values={"amount": "number"})

    @fi.decision
    def invest_cash(ctx):
        cash = ctx.cash.get("USD", 0.0)
        if cash <= 1.0:
            return None
        a = ctx.contracts.index("A")
        return fi.orders({"A": cash / ctx.values()[a]})

    late = ["weight_resets", "decisions", "execution", "capital_flows", "settlement", "recalls", "distribution", "agreements", "cash_guard", "constraints"]
    runs = {}
    for name, order in (("default", None), ("late", late)):
        book = fi.Strategy(invest_cash, schedule="all", execution_lag=0, flows=fi.flows.series("subs.amount"), order=order, **USD)
        runs[name] = ws.run(book, rec)
    cash = {n: r.ledger()["cash"].to_list() for n, r in runs.items()}
    assert cash["default"][10] == pytest.approx(0.0, abs=1e-6), "invested the step it arrived"
    assert cash["late"][10] == pytest.approx(1e7, rel=1e-12) and cash["late"][11] == pytest.approx(0.0, abs=1e-6), "invested a step later"
    assert all(r.max_identity_error < 1e-9 for r in runs.values())
    with pytest.raises(fi.InvalidError, match="execution must follow decisions"):
        fi.Strategy(invest_cash, order=["execution", "decisions", *late[3:], "weight_resets"])
    with pytest.raises(fi.InvalidError, match="no phase"):
        fi.Strategy(invest_cash, order=["lunch"])


def test_m3_cash_in_each_currency_earns_its_own_rate_in_its_own_day_count():
    """Review M3: EUR cash earns €STR on ACT/365F, USD cash SOFR on ACT/360, each credited (so
    compounded) every step; with no rate for EUR, EUR cash earns nothing, as the book then says."""
    ws, rec, dates = world(days=30, flat_b=True)
    ws.series("estr", pl.DataFrame({"date": dates, "rate": [3.0] * 30}), time="date", values={"rate": "%"})

    @fi.decision
    def into_eur_and_out(ctx):
        return {"B": 0.5} if ctx.k == 0 else ({"B": 0.0} if ctx.k == 5 else None)

    def run(deposit):
        return ws.run(fi.Strategy(into_eur_and_out, schedule="all", execution_lag=0, deposit_rate=deposit, **USD), rec)

    flat = run({"USD": "sofr.rate"})
    both = run({"USD": "sofr.rate", "EUR": ("estr.rate", "ACT/365F")})
    eur0 = flat.final_cash["EUR"]
    growth = np.prod([1 + 0.03 * (b - a).days / 365 for a, b in zip(dates[5:-1], dates[6:], strict=True)])
    assert eur0 > 0 and both.final_cash["EUR"] == pytest.approx(eur0 * growth, rel=1e-12)
    usd0 = run(None).final_cash["USD"]
    assert flat.final_cash["USD"] > usd0, "USD cash earned SOFR"
    assert all(r.max_identity_error < 1e-9 for r in (flat, both))
    with pytest.raises(fi.InvalidError, match="coupon periods"):
        ws.run(fi.Strategy(into_eur_and_out, deposit_rate={"EUR": ("estr.rate", "ACT/ACT ICMA")}, **USD), rec)
    with pytest.raises(fi.InvalidError, match="no contract of the book"):
        ws.currency("JPY")
        ws.run(fi.Strategy(into_eur_and_out, deposit_rate={"JPY": "estr.rate"}, **USD), rec)


def test_m4_the_money_weighted_return_is_the_rate_that_prices_the_investors_flows():
    """Review M4: beside the time-weighted return, what the investors earned given their flows."""
    ws, rec, dates = world(days=60)
    years = [(d - dates[0]).days / 365 for d in dates]
    plain = ws.run(fi.Strategy(once({"A": 0.5}), schedule="all", execution_lag=0, **USD), rec)
    nav = plain.ledger()["nav"].to_list()
    assert plain.report()["money_weighted"] == pytest.approx((nav[-1] / nav[0]) ** (1 / years[-1]) - 1, rel=1e-9), "no flows: NAV's growth"
    ws.series("subs", pl.DataFrame({"date": [dates[20], dates[40]], "amount": [5e7, -3e7]}), time="date", values={"amount": "number"})
    run = ws.run(fi.Strategy(once({"A": 0.5}), schedule="all", execution_lag=0, flows=fi.flows.series("subs.amount"), **USD), rec)
    led = run.ledger()
    r = run.report()["money_weighted"]
    nav, cap, costs = led["nav"].to_list(), led["capital"].to_list(), led["costs"].to_list()
    flows = [(0.0, -(nav[0] - cap[0] + costs[0]))] + [(years[k], -c) for k, c in enumerate(cap) if c] + [(years[-1], nav[-1])]
    assert abs(sum(f * (1 + r) ** -t for t, f in flows)) < 1e-3, "the investors' flows are worth nothing at it"


def test_m2_a_books_settings_group_into_what_they_configure():
    """Review M2: grouped settings (cash, shorts, currency) build the same book as the flat ones."""
    ws, rec, dates = world(days=30)
    flat = fi.Strategy(once({"A": 1.2}), schedule="all", execution_lag=0, cash="borrow", borrow_limit=5e7, borrow_rate="sofr.rate", shorts="borrow",
                       borrow_fee_bp=25.0, deposit_rate="sofr.rate", currency="USD", fx="fx.rate")
    grouped = fi.Strategy(once({"A": 1.2}), schedule="all", execution_lag=0, cash=fi.cash.borrow(limit=5e7, rate="sofr.rate", deposit="sofr.rate"),
                          shorts=fi.shorts.borrow(fee_bp=25.0), currency=fi.base_currency("USD", fx="fx.rate"))
    a, b = ws.run(flat, rec), ws.run(grouped, rec)
    assert a.version == b.version and a.ledger()["financing"].sum() > 0, "the same book: it borrowed"
    with pytest.raises(TypeError, match="already sets"):
        fi.Strategy(once({"A": 1.0}), cash=fi.cash.borrow(limit=1.0), borrow_limit=2.0)
    with pytest.raises(ValueError, match="breach"):
        fi.cash.forbid(breach="sometimes")
