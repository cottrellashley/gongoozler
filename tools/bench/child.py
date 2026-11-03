"""One timed backtest, for tools/bench/bench.py: Nautilus's EmaCross over synthetic quotes,
streamed by gongoozler.nautilus.run with progress and snapshots every 250 ms. Writes the run's own
wall time (the simulation only, not loading) to the file named by its first argument."""

from __future__ import annotations

import math
import sys
import time

from nautilus_trader.backtest import BacktestEngine, BacktestEngineConfig
from nautilus_trader.common import LoggerConfig
from nautilus_trader.execution import FixedFeeModel
from nautilus_trader.model import AccountType, BookType, Currency, InstrumentId, Money, OmsType, Price, Quantity, QuoteTick, Symbol, TraderId, Venue
from nautilus_trader.testkit.providers import TestInstrumentProvider
from nautilus_trader.trading import EmaCrossConfig

import gongoozler as gz
from gongoozler import nautilus as gzn

SEC = 1_000_000_000
BASE = 1_546_383_600 * SEC
USD = Currency.from_str("USD")
VENUE = Venue("SIM")


def quotes(n: int) -> list[QuoteTick]:
    iid = InstrumentId(Symbol("AUD/USD"), VENUE)
    out, x = [], 0.71
    for i in range(n):
        x += 0.00002 * math.sin(i / 97.0) + 0.00001 * math.sin(i / 3001.0)
        t = BASE + i * SEC
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


def main() -> None:
    out, n = sys.argv[1], int(sys.argv[2])
    data = quotes(n)
    e = BacktestEngine(BacktestEngineConfig(trader_id=TraderId("BENCH-001"), logging=LoggerConfig(bypass_logging=True), run_analysis=False))
    e.add_venue(
        venue=VENUE,
        oms_type=OmsType.NETTING,
        account_type=AccountType.MARGIN,
        base_currency=USD,
        starting_balances=[Money(1_000_000, USD)],
        fee_model=FixedFeeModel(Money(0, USD)),
        book_type=BookType.L1_MBP,
    )
    inst = TestInstrumentProvider.default_fx_ccy("AUD/USD", VENUE)
    e.add_instrument(inst)
    e.add_builtin_strategy("EmaCross", EmaCrossConfig(instrument_id=inst.id, trade_size=Quantity.from_int(100_000), fast_period=10, slow_period=50))
    feed = gz.from_items("quotes", data, batch_size=50_000, start=BASE, end=data[-1].ts_init)
    # The first event makes the run's ring: a cost of starting (the supervisor reports it as the
    # run's startup), not of running, so it is paid before the clock starts, on both sides.
    gz.log("loaded")
    t0 = time.perf_counter()
    gzn.run(e, feed, every_s=0.25, keep=False)
    took = time.perf_counter() - t0
    e.dispose()
    with open(out, "w") as f:
        f.write(repr(took))


if __name__ == "__main__":
    main()
