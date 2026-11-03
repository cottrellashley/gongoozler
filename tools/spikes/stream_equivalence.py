# /// script
# requires-python = ">=3.13"
# dependencies = ["nautilus_trader==2.0.0rc8"]
# [tool.uv]
# exclude-newer = "2026-11-01T00:00:00Z"
# ///
"""Does Nautilus's streaming loop give the same result as one run? For spec/design.md and kb/measurements.md.

The backtest of progress_cost.py, run once and streamed in 10 and 1,000 batches; compares the
final balance, order count and positions. Run: `uv run tools/spikes/stream_equivalence.py [quotes]`."""

import sys

import progress_cost as s


def final(engine) -> tuple:
    account = engine.cache.account_for_venue(s.VENUE)
    return (account.balance_total(s.USD).as_double(), engine.cache.orders_total_count(), engine.cache.positions_total_count())


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


n = int(sys.argv[1]) if len(sys.argv) > 1 else 300_000
data = s.quotes(n)
one = build()
one.add_data(data)
one.run()
a = final(one)
one.dispose()
for batches in (10, 1_000):
    e = build()
    size = -(-len(data) // batches)
    for i in range(0, len(data), size):
        e.add_data(data[i : i + size])
        e.run(streaming=True)
        e.clear_data()
    e.end()
    b = final(e)
    e.dispose()
    print(f"one run {a}  streamed in {batches} batches {b}  identical: {a == b}")
