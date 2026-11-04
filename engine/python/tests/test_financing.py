"""Financing and collateral from Python (spec, change 5). The Rust tests
(crates/fi-research/tests/financing.rs) hold the full cases; these run them through the user's API."""

from __future__ import annotations

import datetime as dt

import polars as pl
import pytest

import fi_engine as fi

START = dt.date(2025, 3, 3)


def days(n: int) -> list[dt.date]:
    return [START + dt.timedelta(days=k) for k in range(n)]


def bonds(ws: fi.Workspace, dates: list[dt.date], prices: dict[str, float]) -> fi.Contracts:
    names = list(prices)
    ws.series("marks", pl.DataFrame({
        "bond": [n for _ in dates for n in names],
        "date": [d for d in dates for _ in names],
        "clean": [prices[n] for _ in dates for n in names],
        "accrued": [0.0] * (len(dates) * len(names)),
    }), entity="bond", kind="bond", time="date", values={"clean": "price", "accrued": "price"}, grid=True)
    return ws.supplied_bonds("marks")


def repo(cash: float, responses=None):
    ws = fi.Workspace()
    dates = days(6)
    rec = ws.records(bonds(ws, dates, {"B": 100.0}), fi.Calendar(dates))
    ws.series("haircuts", pl.DataFrame({"bond": ["B", "B"], "date": [dates[0], dates[3]], "advance": [0.95, 0.90]}), entity="bond", kind="bond",
              time="date", values={"advance": "number"})
    book = fi.Book([], opening=[("B", 100_000.0)], initial_cash=cash, responses=responses,
                   financing=[fi.Agreement.secured("repo", {"B": "haircuts.advance"}, cure=1, opening=95_000.0, draw=False)])
    return ws.run(book, rec)


def test_r8_a_haircut_increase_is_a_funding_event_not_a_loss():
    run = repo(5_000.0)
    assert run.ledger()["nav"].to_list() == pytest.approx([10_000.0] * 6)
    f = run.financing()
    assert f["capacity"].to_list()[2:4] == pytest.approx([95_000.0, 90_000.0])
    assert f["call"][3] == pytest.approx(5_000.0)
    assert f["balance"][4] == pytest.approx(90_000.0)
    assert run.failed_at is None


def test_r8_an_unmet_call_fails_explicitly_unless_a_declared_response_meets_it():
    run = repo(2_000.0)
    assert run.failed_at == dt.date(2025, 3, 7) or str(run.failed_at).startswith("2025-03-07")
    assert run.shortfall == pytest.approx(3_000.0)
    run = repo(2_000.0, responses=["convert", "draw", ("sell", 50.0)])
    assert run.failed_at is None
    assert run.ledger()["nav"][4] == pytest.approx(6_500.0)


def test_f22_annexes_collateralise_each_netting_set_apart():
    ws = fi.Workspace()
    dates = days(4)
    ws.series("mtm", pl.DataFrame({"swap": ["S1", "S2"] * 4, "date": [d for d in dates for _ in range(2)],
                                   "value": [0.0, 0.0, -10.0, 10.0, -30.0, 30.0, -20.0, 20.0]}),
              entity="swap", kind="swap", time="date", values={"value": "number"}, grid=True)
    rec = ws.records(ws.total_return("S1", "mtm.value") + ws.total_return("S2", "mtm.value"), fi.Calendar(dates))
    book = fi.Book([], opening=[("S1", 1.0), ("S2", 1.0)], initial_cash=100.0,
                   financing=[fi.Agreement.csa("csa1", ["S1"]), fi.Agreement.csa("csa2", ["S2"])])
    run = ws.run(book, rec)
    f = run.financing()
    posted = {a: f.filter(pl.col("agreement") == a)["balance"].to_list() for a in ["csa1", "csa2"]}
    assert posted["csa1"] == pytest.approx([0.0, 10.0, 30.0, 20.0])
    assert posted["csa2"] == pytest.approx([0.0, -10.0, -30.0, -20.0])
    assert run.ledger()["nav"].to_list() == pytest.approx([100.0] * 4), "posting is a receivable, not a loss"


def margin_call(receipt, outstanding: float = 470_000.0):
    """A repo on 500,000 of a bond at par, 92% advanced: the lender assesses margin only at the
    cut, rounding capacity down to whole cents. 4,000 of cash; 7,000 receivable at `receipt`."""
    ws = fi.Workspace()
    dates = days(2)
    cut = dates[1]
    rec = ws.records(bonds(ws, dates, {"G": 100.0}), fi.Calendar(dates))
    agreement = fi.Agreement.secured("repo", {"G": 0.92}, opening=outstanding, draw=False, margin=[cut], rounding=("USD", "down"))
    book = fi.Book([], opening=[("G", 500_000.0)], initial_cash=4_000.0, opening_dues=[(7_000.0, receipt(cut))], responses=[],
                   financing=[agreement])
    return ws.run(book, rec)


def test_a_margin_call_is_paid_from_cash_the_book_can_spend_and_the_rest_stays_owed():
    # The receipt settles five minutes after the cut: it cannot fund the call.
    run = margin_call(lambda cut: dt.datetime.combine(cut, dt.time(0, 5)))
    f = run.financing()
    assert f.columns == ["agreement", "time", "balance", "capacity", "called", "paid", "call", "value"]
    assert f.row(1, named=True) | {"time": None} == {"agreement": "repo", "time": None, "balance": 466_000.0, "capacity": 460_000.0,
                                                     "called": 10_000.0, "paid": 4_000.0, "call": 6_000.0, "value": 500_000.0}
    assert (f["called"][0], f["paid"][0]) == (0.0, 0.0), "no call before the lender's margin time"
    led = run.ledger()
    assert led["cash"].to_list() == [4_000.0, 0.0], "cash does not go negative to pay a call"
    assert led["overdue"].to_list() == [0.0, 6_000.0]
    assert str(run.failed_at).startswith("2025-03-04")
    assert run.shortfall == pytest.approx(6_000.0)
    # Settled by the cut, the receipt pays the call in full.
    run = margin_call(lambda cut: cut)
    assert run.financing()["call"][1] == 0.0
    assert run.ledger()["cash"][1] == pytest.approx(1_000.0)
    assert run.failed_at is None


def test_rounding_terms_take_a_unit_or_a_currency_nautilus_knows():
    fi.Agreement.secured("repo", {"G": 0.9}, rounding=(0.001, "half_even"))
    fi.Agreement.secured("repo", {"G": 0.9}, rounding=("JPY", "down"))
    with pytest.raises(ValueError, match="registry has no BHD"):
        fi.Agreement.secured("repo", {"G": 0.9}, rounding=("BHD", "down"))
    with pytest.raises(ValueError, match="down, up, half_up or half_even"):
        fi.Agreement.secured("repo", {"G": 0.9}, rounding=("USD", "nearest"))
    with pytest.raises(ValueError, match="positive"):
        fi.Agreement.secured("repo", {"G": 0.9}, rounding=(0.0, "down"))


def test_an_opening_due_in_an_unregistered_currency_is_refused():
    ws = fi.Workspace()
    dates = days(2)
    rec = ws.records(bonds(ws, dates, {"G": 100.0}), fi.Calendar(dates))
    with pytest.raises(ValueError, match="no currency EUR"):
        ws.run(fi.Book([], opening=[("G", 1.0)], opening_dues=[(1.0, dates[1], "EUR")]), rec)
