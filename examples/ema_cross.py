# /// script
# requires-python = ">=3.13"
# dependencies = ["gongoozler", "nautilus_trader==2.0.0rc8"]
# [tool.uv.sources]
# gongoozler = { path = "../python", editable = true }
# [tool.uv]
# exclude-newer = "2026-11-01T00:00:00Z"
# ///
"""Nautilus's EMA cross on a month of one-second AUD/USD quotes, streamed from a feed.

The data library here is a stand-in. Any data library defines its reads however it likes, then
hands the backtester a feed of batches sorted by ts_init (spec/feeds.md). This one generates
quotes batch by batch, so the month is never in memory at once.

    uv run examples/ema_cross.py                         # progress printed in the terminal
    gongoozler run examples/ema_cross.py                 # the same file, watched by the CLI
    gongoozler run --param days=5 examples/ema_cross.py  # a parameter set from outside
"""

import math

from nautilus_trader.backtest import BacktestEngine, BacktestEngineConfig
from nautilus_trader.common import LoggerConfig
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

import gongoozler as gz
from gongoozler import nautilus as gzn

SEC = 1_000_000_000
START = gz.to_ns("2024-01-02T00:00:00Z")
USD = Currency.from_str("USD")
VENUE = Venue("SIM")

p = gz.params(days=30, fast=10, slow=50, batch=50_000, tearsheet=False)


def synthetic_quotes(symbol: str, start: int, days: int, batch: int) -> gz.Feed:
    """A stand-in data library: one quote a second, made a batch at a time, with its timestamps
    handed over alongside so the backtester need not read them back."""
    iid = InstrumentId(Symbol(symbol), VENUE)
    n = days * 86_400

    def batches():
        for lo in range(0, n, batch):
            ts = [start + i * SEC for i in range(lo, min(n, lo + batch))]
            items = []
            for t in ts:
                i = (t - start) // SEC
                x = 0.66 + 0.004 * math.sin(i / 1_800) + 0.001 * math.sin(i / 97)
                items.append(QuoteTick(instrument_id=iid, bid_price=Price(x, 5), ask_price=Price(x + 0.0001, 5),
                                       bid_size=Quantity.from_int(1_000_000), ask_size=Quantity.from_int(1_000_000), ts_event=t, ts_init=t))
            yield gz.Batch(items, ts)

    return gz.feed(symbol, batches, start=start, end=start + (n - 1) * SEC)


with gz.stage("set up the engine"):
    engine = BacktestEngine(BacktestEngineConfig(trader_id=TraderId("GZ-001"), logging=LoggerConfig(bypass_logging=True)))
    engine.add_venue(venue=VENUE, oms_type=OmsType.NETTING, account_type=AccountType.MARGIN, base_currency=USD,
                     starting_balances=[Money(1_000_000, USD)], fee_model=FixedFeeModel(Money(0, USD)), book_type=BookType.L1_MBP)
    audusd = TestInstrumentProvider.default_fx_ccy("AUD/USD", VENUE)
    engine.add_instrument(audusd)
    engine.add_builtin_strategy("EmaCross", EmaCrossConfig(instrument_id=audusd.id, trade_size=Quantity.from_int(100_000),
                                                           fast_period=p.fast, slow_period=p.slow))

summary = gzn.run(engine, synthetic_quotes("AUD/USD", START, p.days, p.batch))

with gz.stage("results"):
    balance = engine.cache.account_for_venue(VENUE).balance_total(USD).as_double()
    gz.metric("final balance", balance)
    print(f"{summary.items:,} quotes in {summary.windows} windows, {summary.seconds:.1f}s of simulation; "
          f"{engine.cache.orders_total_count()} orders; final balance {balance:,.2f} USD")
    if p.tearsheet:
        gzn.tearsheet(engine, "tearsheet.html")  # needs nautilus_trader[visualization]
engine.dispose()
