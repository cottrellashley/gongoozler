import pathlib, statistics, sys, tempfile, time
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[2] / "python"))
from tests.test_nautilus import quotes, engine, BASE, SEC, VENUE, USD
import gongoozler as gz
from gongoozler import nautilus as gzn
from nautilus_trader.backtest import BacktestNode, BacktestDataConfig, BacktestEngineConfig, BacktestRunConfig, BacktestVenueConfig
from nautilus_trader.persistence import ParquetDataCatalog, DataCatalogConfig
from nautilus_trader.execution import FixedFeeModel
from nautilus_trader.model import AccountType, BookType, OmsType, NautilusDataType, Quantity, Money, TraderId
from nautilus_trader.common import LoggerConfig
from nautilus_trader.testkit.providers import TestInstrumentProvider
from nautilus_trader.trading import EmaCrossConfig

N = 1_000_000
t = time.perf_counter(); data = quotes("AUD/USD", [BASE + i * SEC for i in range(N)]); build = time.perf_counter() - t
inst = TestInstrumentProvider.default_fx_ccy("AUD/USD", VENUE)
path = tempfile.mkdtemp(prefix="gz-cat-")
cat = ParquetDataCatalog(path)
t = time.perf_counter(); cat.write_instruments([inst]); cat.write_quote_ticks(data); write = time.perf_counter() - t

from nautilus_trader.backtest import BacktestEngine
from nautilus_trader.model import Venue

def plain_engine():
    # As BacktestNode builds it: no recording actor, only the strategy.
    e = BacktestEngine(BacktestEngineConfig(trader_id=TraderId("LOW-001"), logging=LoggerConfig(bypass_logging=True), run_analysis=False))
    e.add_venue(venue=VENUE, oms_type=OmsType.NETTING, account_type=AccountType.MARGIN, base_currency=USD, starting_balances=[Money(1_000_000, USD)],
                fee_model=FixedFeeModel(Money(0, USD)), book_type=BookType.L1_MBP)
    e.add_instrument(inst)
    e.add_builtin_strategy("EmaCross", EmaCrossConfig(instrument_id=inst.id, trade_size=Quantity.from_int(100_000), fast_period=10, slow_period=50))
    return e, None

def from_objects():
    e, r = plain_engine()
    t = time.perf_counter(); gzn.run(e, gz.from_items("q", data, batch_size=100_000), snapshot=False); dt = time.perf_counter() - t
    n = e.cache.orders_total_count(); e.dispose(); return dt, n

def from_catalog():
    venue = BacktestVenueConfig(name="SIM", oms_type=OmsType.NETTING, account_type=AccountType.MARGIN, starting_balances=["1_000_000 USD"],
                                book_type=BookType.L1_MBP, base_currency=USD, fee_model=FixedFeeModel(Money(0, USD)))
    dc = BacktestDataConfig(data_type=NautilusDataType.QuoteTick, catalog=DataCatalogConfig(path=path), instrument_id=inst.id)
    rc = BacktestRunConfig(venues=[venue], data=[dc], engine=BacktestEngineConfig(trader_id=TraderId("NODE-001"), logging=LoggerConfig(bypass_logging=True), run_analysis=False),
                           chunk_size=100_000, dispose_on_completion=False)
    node = BacktestNode([rc]); node.build()
    node.add_builtin_strategy(rc.id, "EmaCross", EmaCrossConfig(instrument_id=inst.id, trade_size=Quantity.from_int(100_000), fast_period=10, slow_period=50))
    t = time.perf_counter(); res = node.run(); dt = time.perf_counter() - t
    n = res[0].total_orders; node.dispose(); return dt, n

print(f"building 1,000,000 QuoteTick objects in Python: {build:.2f}s; writing them to a catalog once: {write:.2f}s")
for name, fn in [("gongoozler driver, Python objects in memory", from_objects), ("BacktestNode streaming the catalog in Rust", from_catalog)]:
    runs = [fn() for _ in range(3)]
    print(f"{name:45} median {statistics.median(r[0] for r in runs):.2f}s per run, orders {runs[0][1]}")
