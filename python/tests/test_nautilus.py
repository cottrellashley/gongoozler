"""The Nautilus driver against Nautilus 2.0.0rc8 itself: a streamed run must equal one run, keep
Nautilus's tie order and timers across feeds, and report progress that only moves forward."""

from __future__ import annotations

import math

import pytest

nautilus_trader = pytest.importorskip("nautilus_trader")

from nautilus_trader.backtest import BacktestEngine, BacktestEngineConfig  # noqa: E402
from nautilus_trader.common import DataActor, DataActorConfig, LoggerConfig  # noqa: E402
from nautilus_trader.execution import FixedFeeModel  # noqa: E402
from nautilus_trader.model import (  # noqa: E402
    AccountType,
    BookType,
    Currency,
    InstrumentId,
    Money,
    OmsType,
    Price,
    Quantity,
    QuoteTick,
    Symbol,
    TraderId,
    Venue,
)
from nautilus_trader.testkit.providers import TestInstrumentProvider  # noqa: E402
from nautilus_trader.trading import EmaCrossConfig  # noqa: E402

import gongoozler as gz  # noqa: E402
from gongoozler import nautilus as gzn  # noqa: E402

from .conftest import Recording  # noqa: E402

SEC = 1_000_000_000
BASE = 1_546_383_600 * SEC
USD = Currency.from_str("USD")
VENUE = Venue("SIM")


def quotes(symbol: str, times: list[int], phase: float = 0.0) -> list[QuoteTick]:
    iid = InstrumentId(Symbol(symbol), VENUE)
    out = []
    for i, t in enumerate(times):
        x = 0.71 + 0.003 * math.sin(i / 97.0 + phase) + 0.001 * math.sin(i / 1201.0)
        out.append(
            QuoteTick(
                instrument_id=iid,
                bid_price=Price(x, 5),
                ask_price=Price(x + 0.0001, 5),
                bid_size=Quantity.from_int(1_000_000),
                ask_size=Quantity.from_int(1_000_000),
                ts_event=t,
                ts_init=t,
            )
        )
    return out


class Recorder(DataActor):
    """Records each quote it sees, in order, and counts a simulated-time timer."""

    def __init__(self, ids: list[InstrumentId]) -> None:  # Nautilus 2.0 actors take at most one argument
        super().__init__(DataActorConfig(log_events=False, log_commands=False))
        self.ids = ids
        self.seen: list[tuple[int, str]] = []
        self.ticks = 0

    def on_start(self) -> None:
        for iid in self.ids:
            self.subscribe_quotes(iid)
        self.clock.set_timer_ns("recorder", 37 * SEC, start_time_ns=BASE + 37 * SEC, callback=None)

    def on_quote(self, quote: QuoteTick) -> None:
        self.seen.append((quote.ts_init, str(quote.instrument_id)))

    def on_time_event(self, event) -> None:  # type: ignore[no-untyped-def]
        self.ticks += 1


def engine(symbols: list[str]) -> tuple[BacktestEngine, Recorder]:
    e = BacktestEngine(BacktestEngineConfig(trader_id=TraderId("TEST-001"), logging=LoggerConfig(bypass_logging=True), run_analysis=False))
    e.add_venue(
        venue=VENUE,
        oms_type=OmsType.NETTING,
        account_type=AccountType.MARGIN,
        base_currency=USD,
        starting_balances=[Money(1_000_000, USD)],
        fee_model=FixedFeeModel(Money(0, USD)),
        book_type=BookType.L1_MBP,
    )
    instruments = [TestInstrumentProvider.default_fx_ccy(s, VENUE) for s in symbols]
    for inst in instruments:
        e.add_instrument(inst)
    e.add_builtin_strategy("EmaCross", EmaCrossConfig(instrument_id=instruments[0].id, trade_size=Quantity.from_int(100_000), fast_period=10, slow_period=50))
    rec = Recorder([inst.id for inst in instruments])
    e.add_actor(rec)
    return e, rec


def outcome(e: BacktestEngine, rec: Recorder) -> tuple:
    account = e.cache.account_for_venue(VENUE)
    return (account.balance_total(USD).as_double(), e.cache.orders_total_count(), e.cache.positions_total_count(), rec.ticks, rec.seen)


def two_feeds() -> tuple[list[QuoteTick], list[QuoteTick]]:
    """AUD/USD every second; EUR/USD every second second and on every 7th, so the two often tie."""
    aud = quotes("AUD/USD", [BASE + i * SEC for i in range(20_000)])
    eur = quotes("EUR/USD", sorted({BASE + i * SEC for i in range(0, 20_000, 2)} | {BASE + i * SEC for i in range(0, 20_000, 7)}), 1.3)
    return aud, eur


def test_a_streamed_run_equals_one_run() -> None:
    data = quotes("AUD/USD", [BASE + i * SEC for i in range(50_000)])
    one, r1 = engine(["AUD/USD"])
    one.add_data(data)
    one.run()
    expected = outcome(one, r1)
    one.dispose()
    streamed, r2 = engine(["AUD/USD"])
    summary = gzn.run(streamed, gz.from_items("quotes", data, batch_size=7_919), snapshot=False)
    assert outcome(streamed, r2) == expected
    assert expected[1] > 50, "the strategy traded"
    assert (summary.windows, summary.items) == (7, 50_000)
    streamed.dispose()


def test_two_feeds_keep_nautilus_tie_order_and_timers() -> None:
    aud, eur = two_feeds()
    one, r1 = engine(["AUD/USD", "EUR/USD"])
    one.add_data(aud)
    one.add_data(eur)
    one.run()
    expected = outcome(one, r1)
    one.dispose()
    ties = sum(1 for a, b in zip(expected[4], expected[4][1:], strict=False) if a[0] == b[0])
    assert ties > 5_000, "the case exercises equal timestamps"
    streamed, r2 = engine(["AUD/USD", "EUR/USD"])
    gzn.run(streamed, [gz.from_items("aud", aud, batch_size=997), gz.from_items("eur", eur, batch_size=331)], snapshot=False)
    assert outcome(streamed, r2) == expected
    streamed.dispose()


def test_progress_only_moves_forward_and_ends_at_the_end(recorded: Recording) -> None:
    data = quotes("AUD/USD", [BASE + i * SEC for i in range(30_000)])
    e, _ = engine(["AUD/USD"])
    gzn.run(e, gz.from_items("quotes", data, batch_size=1_000, start=BASE, end=data[-1].ts_init), every_s=0.0)
    e.dispose()
    progress = recorded.of("progress")
    assert len(progress) >= 2
    done = [p["done"] for p in progress]
    assert done == sorted(done)
    assert progress[-1]["done"] == progress[-1]["total"] == data[-1].ts_init - BASE
    assert progress[-1]["sim"] == data[-1].ts_init and progress[-1]["unit"] == "ns"
    names = {m["name"] for m in recorded.of("metric")}
    assert {"balance.SIM.USD", "positions.open", "orders"} <= names
    assert [(s["name"], s["phase"], s.get("ok")) for s in recorded.of("stage")] == [("simulation", "begin", None), ("simulation", "end", True)]


def test_without_a_declared_end_progress_counts_items(recorded: Recording) -> None:
    data = quotes("AUD/USD", [BASE + i * SEC for i in range(5_000)])
    e, _ = engine(["AUD/USD"])
    gzn.run(e, gz.from_items("quotes", data, batch_size=1_000), every_s=0.0, snapshot=False)
    e.dispose()
    last = recorded.of("progress")[-1]
    assert (last["done"], last["unit"], "total" in last) == (5_000, "items", False)


def test_a_feed_that_goes_back_in_time_stops_the_run(recorded: Recording) -> None:
    data = quotes("AUD/USD", [BASE + i * SEC for i in range(3_000)])
    broken = gz.feed("quotes", [data[1_000:2_000], data[:1_000]])
    e, _ = engine(["AUD/USD"])
    with pytest.raises(gz.FeedError, match="feed 'quotes', batch 1: goes back in time"):
        gzn.run(e, broken)
    e.dispose()
    assert recorded.of("stage")[-1]["ok"] is False
