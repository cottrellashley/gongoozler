# /// script
# requires-python = ">=3.13"
# dependencies = ["nautilus_trader==2.0.0rc8"]
# [tool.uv]
# exclude-newer = "2026-11-01T00:00:00Z"
# ///
"""What does reading progress cost inside a Nautilus backtest (2.0.0rc8)? For spec/design.md and kb/measurements.md.

Run: `uv run tools/spikes/progress_cost.py [quotes]` (default 500,000).

The same backtest (Nautilus's built-in EmaCross on synthetic AUD/USD quotes) runs without an
actor, then with an actor that reports simulated-time progress and the account's balance on a
simulated-time timer. The actor appends to an in-memory buffer only when 250 ms of wall clock
have passed (the emitter's contract: never block the engine). Also streams the same data in batches, reading progress between them with nothing in the loop.
Prints wall times for loading the data and running.
"""

from __future__ import annotations

import collections
import math
import statistics
import sys
import time

from nautilus_trader.backtest import BacktestEngine, BacktestEngineConfig
from nautilus_trader.common import DataActor, DataActorConfig, LoggerConfig
from nautilus_trader.execution import FixedFeeModel
from nautilus_trader.model import (
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
from nautilus_trader.testkit.providers import TestInstrumentProvider
from nautilus_trader.trading import EmaCrossConfig

N = int(sys.argv[1]) if len(sys.argv) > 1 else 500_000
REPEATS = 3
SEC = 1_000_000_000
BASE = 1_546_383_600 * SEC
USD = Currency.from_str("USD")
VENUE = Venue("SIM")


def quotes(n: int) -> list:
    iid = InstrumentId(Symbol("AUD/USD"), VENUE)
    out = []
    x = 0.71
    for i in range(n):
        x += 0.00002 * math.sin(i / 97.0) + 0.00001 * math.sin(i / 3001.0)
        out.append(
            QuoteTick(
                instrument_id=iid,
                bid_price=Price(x, 5),
                ask_price=Price(x + 0.0001, 5),
                bid_size=Quantity.from_int(1_000_000),
                ask_size=Quantity.from_int(1_000_000),
                ts_event=BASE + i * SEC,
                ts_init=BASE + i * SEC,
            )
        )
    return out


class ProgressActor(DataActor):
    """Reports how far through the simulated window the run is, throttled to wall clock."""

    # A Nautilus 2.0 actor is a Rust class: its constructor takes at most one argument.
    def __init__(self, settings: tuple) -> None:
        super().__init__(DataActorConfig(log_events=False, log_commands=False))
        self.start_ns, self.end_ns, self.every_ns, self.sink = settings
        self.last = 0.0
        self.ticks = 0

    def on_start(self) -> None:
        if self.every_ns:
            self.clock.set_timer_ns("lab.progress", self.every_ns, start_time_ns=self.start_ns + self.every_ns, callback=None)

    def on_time_event(self, event) -> None:
        self.ticks += 1
        now = time.monotonic()
        if now - self.last < 0.25:
            return
        self.last = now
        sim = self.clock.timestamp_ns()
        account = self.cache.account_for_venue(VENUE)
        balance = account.balance_total(USD) if account is not None else None
        self.sink.append(
            {
                "kind": "progress",
                "sim": sim,
                "done": (sim - self.start_ns) / (self.end_ns - self.start_ns),
                "balance": None if balance is None else balance.as_double(),
                "open": self.cache.positions_open_count(),
            }
        )


def run_streamed(data: list, batches: int) -> tuple[float, int, int]:
    """Nautilus's streaming loop: progress is read between batches, with nothing in the loop."""
    engine = BacktestEngine(BacktestEngineConfig(trader_id=TraderId("SPIKE-001"), logging=LoggerConfig(bypass_logging=True), run_analysis=False))
    engine.add_venue(
        venue=VENUE,
        oms_type=OmsType.NETTING,
        account_type=AccountType.MARGIN,
        base_currency=USD,
        starting_balances=[Money(1_000_000, USD)],
        fee_model=FixedFeeModel(Money(0, USD)),
        book_type=BookType.L1_MBP,
    )
    instrument = TestInstrumentProvider.default_fx_ccy("AUD/USD", VENUE)
    engine.add_instrument(instrument)
    engine.add_builtin_strategy("EmaCross", EmaCrossConfig(instrument_id=instrument.id, trade_size=Quantity.from_int(100_000), fast_period=10, slow_period=50))
    sink: collections.deque = collections.deque(maxlen=10_000)
    size = math.ceil(len(data) / batches)
    start, end = BASE, BASE + (len(data) - 1) * SEC
    last = 0.0
    t0 = time.perf_counter()
    for i in range(0, len(data), size):
        batch = data[i : i + size]
        engine.add_data(batch)
        engine.run(streaming=True)
        engine.clear_data()
        now = time.monotonic()
        if now - last >= 0.25:
            last = now
            account = engine.cache.account_for_venue(VENUE)
            balance = account.balance_total(USD) if account is not None else None
            sim = batch[-1].ts_init
            sink.append(
                {
                    "kind": "progress",
                    "sim": sim,
                    "done": (sim - start) / (end - start),
                    "balance": None if balance is None else balance.as_double(),
                    "open": engine.cache.positions_open_count(),
                }
            )
    engine.end()
    elapsed = time.perf_counter() - t0
    engine.dispose()
    return elapsed, len(sink), batches


def run(data: list, actor_every_ns: int | None) -> tuple[float, int, int]:
    engine = BacktestEngine(BacktestEngineConfig(trader_id=TraderId("SPIKE-001"), logging=LoggerConfig(bypass_logging=True), run_analysis=False))
    engine.add_venue(
        venue=VENUE,
        oms_type=OmsType.NETTING,
        account_type=AccountType.MARGIN,
        base_currency=USD,
        starting_balances=[Money(1_000_000, USD)],
        fee_model=FixedFeeModel(Money(0, USD)),
        book_type=BookType.L1_MBP,
    )
    instrument = TestInstrumentProvider.default_fx_ccy("AUD/USD", VENUE)
    engine.add_instrument(instrument)
    engine.add_builtin_strategy("EmaCross", EmaCrossConfig(instrument_id=instrument.id, trade_size=Quantity.from_int(100_000), fast_period=10, slow_period=50))
    sink: collections.deque = collections.deque(maxlen=10_000)
    actor = None
    if actor_every_ns is not None:
        actor = ProgressActor((BASE, BASE + (len(data) - 1) * SEC, actor_every_ns, sink))
        engine.add_actor(actor)
    t0 = time.perf_counter()  # loading the data counts, as it does when streaming
    engine.add_data(data)
    engine.run()
    elapsed = time.perf_counter() - t0
    fills = engine.cache.orders_total_count() if hasattr(engine, "cache") else -1
    ticks = actor.ticks if actor is not None else 0
    engine.dispose()
    return elapsed, len(sink), ticks if actor is not None else fills


def main() -> None:
    t = time.perf_counter()
    data = quotes(N)
    print(f"{N} quotes ({N / 86_400:.1f} simulated days) built in {time.perf_counter() - t:.1f}s")
    window = (N - 1) * SEC
    cases = [
        ("no actor", None),
        ("actor, no timer", 0),
        ("actor, 1,000 timer events", window // 1_000),
        ("actor, 10,000 timer events", window // 10_000),
        ("actor, timer every simulated second", SEC),
        ("streamed in 100 batches, no actor", -100),
        ("streamed in 1,000 batches, no actor", -1_000),
    ]
    results = {}
    for name, every in cases:
        times = []
        for _ in range(REPEATS):
            elapsed, emitted, ticks = run_streamed(data, -every) if every is not None and every < 0 else run(data, every)
            times.append(elapsed)
        results[name] = statistics.median(times)
        print(
            f"{name:38} median {results[name]:.3f}s  runs {[round(x, 3) for x in times]}  emitted {emitted}  timer events {ticks if every is not None else '-'}"
        )
    base = results["no actor"]
    for name, _ in cases[1:]:
        print(f"overhead, {name}: {100 * (results[name] / base - 1):+.1f}%")


if __name__ == "__main__":
    main()
