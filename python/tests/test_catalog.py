"""Catalog feeds (spec/feeds.md, "Catalog feeds"): a feed written once to a Parquet catalog and
streamed by BacktestNode in Rust trades exactly as the same feed streamed as Python objects."""

from __future__ import annotations

import json
import threading
from pathlib import Path

import pytest

pytest.importorskip("nautilus_trader")

from nautilus_trader.backtest import BacktestEngine, BacktestEngineConfig, BacktestVenueConfig
from nautilus_trader.common import LoggerConfig
from nautilus_trader.execution import FixedFeeModel
from nautilus_trader.model import AccountType, Bar, BarType, BookType, Money, OmsType, Price, Quantity, TraderId
from nautilus_trader.persistence import ParquetDataCatalog
from nautilus_trader.testkit.providers import TestInstrumentProvider
from nautilus_trader.trading import EmaCrossConfig

import gongoozler as gz
from gongoozler import catalog
from gongoozler import nautilus as gzn

from .conftest import Recording
from .test_nautilus import BASE, SEC, USD, VENUE, quotes

AUD = TestInstrumentProvider.default_fx_ccy("AUD/USD", VENUE)
STRATEGY = ("EmaCross", EmaCrossConfig(instrument_id=AUD.id, trade_size=Quantity.from_int(100_000), fast_period=10, slow_period=50))
SIM = BacktestVenueConfig(
    name="SIM",
    oms_type=OmsType.NETTING,
    account_type=AccountType.MARGIN,
    starting_balances=["1_000_000 USD"],
    book_type=BookType.L1_MBP,
    base_currency=USD,
    fee_model=FixedFeeModel(Money(0, USD)),
)


def the_feed(n: int = 20_000) -> gz.Feed:
    data = quotes("AUD/USD", [BASE + i * SEC for i in range(n)])
    return gz.from_items("quotes", data, batch_size=3_000, start=BASE, end=data[-1].ts_init, kind="QuoteTick", instruments=["AUD/USD.SIM"])


def streamed(feed: gz.Feed) -> None:
    """The same venue and strategy as SIM and STRATEGY, built by hand and fed Python objects."""
    e = BacktestEngine(BacktestEngineConfig(trader_id=TraderId("GONGOOZLER-001"), logging=LoggerConfig(bypass_logging=True)))
    e.add_venue(
        venue=VENUE,
        oms_type=OmsType.NETTING,
        account_type=AccountType.MARGIN,
        base_currency=USD,
        starting_balances=[Money(1_000_000, USD)],
        fee_model=FixedFeeModel(Money(0, USD)),
        book_type=BookType.L1_MBP,
    )
    e.add_instrument(AUD)
    e.add_builtin_strategy(*STRATEGY)
    gzn.run(e, feed, snapshot=False)
    e.dispose()


def kept(d: Path) -> dict[str, list]:
    return {p.name: [json.loads(x) for x in p.read_text().splitlines()] for p in sorted(d.iterdir()) if p.name != "manifest.json"}


def test_a_catalog_run_trades_exactly_as_the_streamed_run(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    feed = the_feed()
    monkeypatch.setenv("GONGOOZLER_ARTIFACTS", str(tmp_path / "objects"))
    streamed(feed)
    feeds = catalog.write(tmp_path / "cat", feed, instruments=[AUD], rows_per_file=7_000)
    monkeypatch.setenv("GONGOOZLER_ARTIFACTS", str(tmp_path / "node"))
    result = gzn.run_node(venues=[SIM], data=feeds, strategies=[STRATEGY])
    a, b = kept(tmp_path / "objects"), kept(tmp_path / "node")
    assert set(a) == set(b) and len(a["fills.jsonl"]) > 50
    assert a == b, "the same orders, fills, positions, account, returns, statistics and series"
    assert result.total_orders == len(a["orders.jsonl"])
    objects = json.loads((tmp_path / "objects" / "manifest.json").read_text())["inputs"]["feeds"][0]
    node = json.loads((tmp_path / "node" / "manifest.json").read_text())["inputs"]["feeds"][0]
    assert node["source"] == "catalog" and node["fingerprint"] == objects["fingerprint"], "the catalog carries the feed's fingerprint"


def test_writing_a_catalog_keeps_each_timestamp_in_one_file(tmp_path: Path) -> None:
    data = quotes("AUD/USD", [BASE + (i // 3) * SEC for i in range(3_000)])  # three quotes at every timestamp
    (cf,) = catalog.write(tmp_path, gz.from_items("q", data, batch_size=100), rows_per_file=100)
    assert (cf.data_type, cf.instrument_ids, cf.count, cf.start, cf.end) == ("QuoteTick", ["AUD/USD.SIM"], 3_000, BASE, data[-1].ts_init)
    assert len(ParquetDataCatalog(str(tmp_path)).query_quote_ticks()) == 3_000
    assert len(list((tmp_path / "data" / "quotes").rglob("*.parquet"))) == 30


def test_the_node_reports_progress_that_only_moves_forward(tmp_path: Path, recorded: Recording) -> None:
    feeds = catalog.write(tmp_path, the_feed(5_000), instruments=[AUD])
    gzn.run_node(venues=[SIM], data=feeds, strategies=[STRATEGY], updates=40, keep=False)
    progress = recorded.of("progress")
    done = [p["done"] for p in progress]
    assert 30 <= len(progress) <= 45 and done == sorted(done)
    assert progress[-1]["done"] == progress[-1]["total"] == feeds[0].end - feeds[0].start and progress[-1]["unit"] == "ns"
    assert [(s["name"], s["phase"], s.get("ok")) for s in recorded.of("stage")] == [("simulation", "begin", None), ("simulation", "end", True)]


def test_a_cached_feed_is_built_once(tmp_path: Path) -> None:
    builds = []

    def build() -> gz.Feed:
        builds.append(1)
        return the_feed(2_000)

    first = catalog.cached(tmp_path, "aud quotes 2019-01-01", build, instruments=lambda: [AUD])
    again = catalog.cached(tmp_path, "aud quotes 2019-01-01", build, instruments=lambda: [AUD])
    assert first == again and len(builds) == 1
    assert first[0].key == "aud quotes 2019-01-01" and Path(first[0].path).parent == tmp_path
    other = catalog.cached(tmp_path, "aud quotes, another read", build)
    assert len(builds) == 2 and other[0].path != first[0].path, "each key is a catalog of its own"


def test_a_cached_feed_left_half_written_is_written_again(tmp_path: Path) -> None:
    def broken() -> gz.Feed:
        data = quotes("AUD/USD", [BASE + i * SEC for i in range(2_000)])
        return gz.feed("quotes", [data[:1_000], data[:10]])

    with pytest.raises(gz.FeedError):
        catalog.cached(tmp_path, "k", broken)
    (cf,) = catalog.cached(tmp_path, "k", lambda: the_feed(2_000))
    assert cf.count == 2_000 and len(ParquetDataCatalog(cf.path).query_quote_ticks()) == 2_000


def test_runs_asking_for_a_cached_feed_at_once_build_it_once(tmp_path: Path) -> None:
    builds, out = [], []

    def build() -> gz.Feed:
        builds.append(1)
        return the_feed(2_000)

    threads = [threading.Thread(target=lambda: out.append(catalog.cached(tmp_path, "k", build))) for _ in range(4)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    assert len(builds) == 1 and len(out) == 4 and all(o == out[0] for o in out)


def test_bars_are_named_by_their_bar_types(tmp_path: Path) -> None:
    bt = BarType.from_str("AUD/USD.SIM-1-MINUTE-BID-EXTERNAL")
    p = Price.from_str("0.71000")
    bars = [Bar(bt, p, p, p, p, Quantity.from_int(1), BASE + i * 60 * SEC, BASE + i * 60 * SEC) for i in range(100)]
    (cf,) = catalog.write(tmp_path, gz.from_items("bars", bars))
    assert (cf.data_type, cf.bar_types, cf.instrument_ids, cf.count) == ("Bar", [str(bt)], ["AUD/USD.SIM"], 100)
    assert "bar_types" in repr(cf.data_config()) and len(ParquetDataCatalog(str(tmp_path)).query_bars()) == 100


def test_a_catalog_holds_only_nautilus_data(tmp_path: Path) -> None:
    class Row:
        ts_init = 1

    with pytest.raises(TypeError, match="a catalog cannot hold Row items"):
        catalog.write(tmp_path, gz.feed("rows", [[Row()]]))
