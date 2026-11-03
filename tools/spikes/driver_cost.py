# /// script
# requires-python = ">=3.13"
# dependencies = ["gongoozler", "nautilus_trader==2.0.0rc8"]
# [tool.uv.sources]
# gongoozler = { path = "../../python", editable = true }
# [tool.uv]
# exclude-newer = "2026-11-01T00:00:00Z"
# ///
"""What does gongoozler's Nautilus driver cost over Nautilus's own streaming loop? For kb/measurements.md.

The backtest of progress_cost.py (built-in EmaCross, synthetic AUD/USD quotes, built in memory first)
runs four ways, each streamed in 100 windows: a hand-written loop with no progress; and
`gongoozler.nautilus.run` checking every timestamp (read from the items), checking every timestamp
(handed over by the feed), and checking only where batches meet. Prints medians of three.

Run: `GONGOOZLER_EVENTS=off uv run tools/spikes/driver_cost.py [quotes]` (default 1,000,000).
"""

import statistics
import sys
import time

import progress_cost as s

import gongoozler as gz
from gongoozler import nautilus as gzn

N = int(sys.argv[1]) if len(sys.argv) > 1 else 1_000_000
WINDOWS = 100


def build():
    engine = s.BacktestEngine(s.BacktestEngineConfig(trader_id=s.TraderId("SPIKE-001"), logging=s.LoggerConfig(bypass_logging=True), run_analysis=False))
    engine.add_venue(
        venue=s.VENUE,
        oms_type=s.OmsType.NETTING,
        account_type=s.AccountType.MARGIN,
        base_currency=s.USD,
        starting_balances=[s.Money(1_000_000, s.USD)],
        fee_model=s.FixedFeeModel(s.Money(0, s.USD)),
        book_type=s.BookType.L1_MBP,
    )
    instrument = s.TestInstrumentProvider.default_fx_ccy("AUD/USD", s.VENUE)
    engine.add_instrument(instrument)
    engine.add_builtin_strategy(
        "EmaCross", s.EmaCrossConfig(instrument_id=instrument.id, trade_size=s.Quantity.from_int(100_000), fast_period=10, slow_period=50)
    )
    return engine


def bare(data, size):
    e = build()
    t = time.perf_counter()
    for i in range(0, len(data), size):
        e.add_data(data[i : i + size])
        e.run(streaming=True)
        e.clear_data()
    e.end()
    elapsed = time.perf_counter() - t
    e.dispose()
    return elapsed


def driver(data, size, check, ts):
    e = build()
    f = gz.from_items("quotes", data, batch_size=size, ts=ts, start=data[0].ts_init, end=data[-1].ts_init)
    t = time.perf_counter()
    gzn.run(e, f, check=check, keep=False)
    elapsed = time.perf_counter() - t
    e.dispose()
    return elapsed


def main():
    data = s.quotes(N)
    ts = [q.ts_init for q in data]
    size = -(-N // WINDOWS)
    cases = {
        "hand-written loop, no progress": lambda: bare(data, size),
        "driver, timestamps read from items": lambda: driver(data, size, "full", None),
        "driver, timestamps handed over": lambda: driver(data, size, "full", ts),
        "driver, edges only": lambda: driver(data, size, "edges", None),
    }
    base = None
    for name, fn in cases.items():
        times = [fn() for _ in range(3)]
        m = statistics.median(times)
        base = base or m
        print(f"{name:38} median {m:.3f}s  runs {[round(x, 3) for x in times]}  {100 * (m / base - 1):+.1f}%")


if __name__ == "__main__":
    main()
