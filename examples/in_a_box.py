# /// script
# requires-python = ">=3.13"
# dependencies = ["gongoozler", "nautilus_trader==2.0.0rc8"]
# [tool.uv]
# exclude-newer = "2026-11-01T00:00:00Z"
# [tool.gongoozler]
# memory = "2GiB"
# cpus = 1
# timeout = "10m"
# ///
"""A file for a box: it names gongoozler with no path, as a box resolves it (Dockerfile). The
same backtest as examples/ema_cross.py, a day of it, with results kept.

    gongoozler run --remote examples/in_a_box.py     # on the server, in a container of its own
"""

import math

from nautilus_trader.backtest import BacktestEngine, BacktestEngineConfig
from nautilus_trader.common import LoggerConfig
from nautilus_trader.execution import FixedFeeModel
from nautilus_trader.model import AccountType, BookType, Currency, InstrumentId, Money, OmsType, Price, Quantity, QuoteTick, Symbol, TraderId, Venue
from nautilus_trader.testkit.providers import TestInstrumentProvider
from nautilus_trader.trading import EmaCrossConfig

import gongoozler as gz
from gongoozler import nautilus as gzn

SEC = 1_000_000_000
START = gz.to_ns("2019-01-01T00:00:00Z")
p = gz.params(hours=24, fast=10, slow=50)
USD = Currency.from_str("USD")
SIM = Venue("SIM")
END = START + p.hours * 3600 * SEC


def quotes():  # a stand-in data library: an hour of one-second quotes a batch
    iid = InstrumentId(Symbol("AUD/USD"), SIM)
    for h in range(p.hours):
        out = []
        for s in range(3600):
            i = h * 3600 + s
            x = 0.71 + 0.003 * math.sin(i / 997.0)
            t = START + i * SEC
            out.append(QuoteTick(instrument_id=iid, bid_price=Price(x, 5), ask_price=Price(x + 0.0001, 5), bid_size=Quantity.from_int(1_000_000),
                                 ask_size=Quantity.from_int(1_000_000), ts_event=t, ts_init=t))
        yield out


engine = BacktestEngine(BacktestEngineConfig(trader_id=TraderId("BOX-001"), logging=LoggerConfig(bypass_logging=True)))
engine.add_venue(venue=SIM, oms_type=OmsType.NETTING, account_type=AccountType.MARGIN, base_currency=USD, starting_balances=[Money(1_000_000, USD)],
                 fee_model=FixedFeeModel(Money(0, USD)), book_type=BookType.L1_MBP)
audusd = TestInstrumentProvider.default_fx_ccy("AUD/USD", SIM)
engine.add_instrument(audusd)
engine.add_builtin_strategy("EmaCross", EmaCrossConfig(instrument_id=audusd.id, trade_size=Quantity.from_int(100_000), fast_period=p.fast, slow_period=p.slow))
gzn.run(engine, gz.feed("quotes", quotes, start=START, end=END - SEC, kind="QuoteTick", instruments=[str(audusd.id)]))
engine.dispose()
