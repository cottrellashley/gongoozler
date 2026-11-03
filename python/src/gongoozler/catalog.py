"""Feeds in Nautilus's Parquet catalogs: pull once, run many (spec/feeds.md, "Catalog feeds").

Building data as Python objects costs about as much as simulating it, and a sweep's every run
builds it again. Written once to a `ParquetDataCatalog`, the same data streams into
`BacktestNode` in Rust, with no Python object per item: about 2x cheaper per run, with memory
bounded by Nautilus's chunks (kb/measurements.md).

- `write(path, feed)` writes any feed's items into a catalog, by data type, and returns what it
  wrote as `CatalogFeed`s for `gongoozler.nautilus.run_node`.
- `cached(root, key, build)` writes a key's data only the first time any run asks for it: the data
  library's reads run once, and every later run, or every run of a sweep, reads the catalog.

Importing this module needs nautilus_trader.
"""

from __future__ import annotations

import contextlib
import fcntl
import hashlib
import json
import os
import re
import shutil
from collections.abc import Callable, Iterator, Sequence
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Any

from .feeds import Feed, windows

# The item types a catalog holds: the catalog method that writes them, and the data type a
# BacktestDataConfig reads them as.
WRITERS: dict[str, tuple[str, str]] = {
    "QuoteTick": ("write_quote_ticks", "QuoteTick"),
    "TradeTick": ("write_trade_ticks", "TradeTick"),
    "Bar": ("write_bars", "Bar"),
    "OrderBookDelta": ("write_order_book_deltas", "OrderBookDelta"),
    "OrderBookDepth10": ("write_order_book_depths", "OrderBookDepth"),
    "MarkPriceUpdate": ("write_mark_price_updates", "MarkPriceUpdate"),
    "IndexPriceUpdate": ("write_index_price_updates", "IndexPriceUpdate"),
    "OptionGreeks": ("write_option_greeks", "OptionGreeks"),
    "InstrumentStatus": ("write_instrument_statuses", "InstrumentStatus"),
    "InstrumentClose": ("write_instrument_closes", "InstrumentClose"),
}
META = "gongoozler-feeds.json"


@dataclass
class CatalogFeed:
    """Data of one type in a catalog, as written: what `run_node` streams. Bars are named by their
    bar types, everything else by instrument."""

    path: str
    data_type: str
    instrument_ids: list[str]
    start: int | None
    end: int | None
    count: int
    bar_types: list[str] = field(default_factory=list)
    fingerprint: dict[str, Any] = field(default_factory=dict)
    key: str | None = None
    revision: str | None = None

    def data_config(self) -> Any:
        """A `BacktestDataConfig` that reads this data."""
        from nautilus_trader.backtest import BacktestDataConfig
        from nautilus_trader.model import InstrumentId, NautilusDataType
        from nautilus_trader.persistence import DataCatalogConfig

        kw: dict[str, Any] = {"data_type": getattr(NautilusDataType, self.data_type), "catalog": DataCatalogConfig(path=self.path)}
        if self.bar_types:
            kw["bar_types"] = list(self.bar_types)
        else:
            ids = [InstrumentId.from_str(i) for i in self.instrument_ids]
            if len(ids) == 1:
                kw["instrument_id"] = ids[0]
            elif ids:
                kw["instrument_ids"] = ids
        return BacktestDataConfig(**kw)

    def describe(self) -> dict[str, Any]:
        """The feed for a run's manifest."""
        return {"source": "catalog", **asdict(self)}


def write(
    path: str | os.PathLike[str],
    feed: Feed,
    *,
    instruments: Sequence[Any] = (),
    rows_per_file: int = 1_000_000,
    key: str | None = None,
) -> list[CatalogFeed]:
    """Writes `feed`'s items into the catalog at `path` (made if need be), by data type, with the
    Nautilus `instruments` the runs need. The feed is checked and fingerprinted as it is read, and
    never held whole: items are written about `rows_per_file` to a file, a file never splitting a
    timestamp (the catalog's files must not overlap). Returns a `CatalogFeed` per data type."""
    from nautilus_trader.persistence import ParquetDataCatalog

    root = Path(path).resolve()
    root.mkdir(parents=True, exist_ok=True)
    cat = ParquetDataCatalog(str(root))
    if instruments:
        cat.write_instruments(list(instruments))
    pending: dict[str, list[Any]] = {}
    seen: dict[str, _Seen] = {}
    for w in windows([feed]):
        for _, items in w.parts:
            for x in items:
                t = type(x).__name__
                if t not in WRITERS:
                    raise TypeError(f"feed {feed.name!r}: a catalog cannot hold {t} items (it holds {', '.join(sorted(WRITERS))})")
                rows = pending.setdefault(t, [])
                if len(rows) >= rows_per_file and x.ts_init != rows[-1].ts_init:
                    getattr(cat, WRITERS[t][0])(rows)
                    rows = pending[t] = []
                rows.append(x)
                seen.setdefault(t, _Seen()).add(x)
    for t, rows in pending.items():
        if rows:
            getattr(cat, WRITERS[t][0])(rows)
    fp = feed.fingerprint.as_dict()
    return [
        CatalogFeed(
            path=str(root),
            data_type=WRITERS[t][1],
            instrument_ids=sorted(s.ids),
            start=s.start,
            end=s.end,
            count=s.count,
            bar_types=sorted(s.bar_types),
            fingerprint=fp,
            key=key,
            revision=feed.revision,
        )
        for t, s in sorted(seen.items())
    ]


class _Seen:
    __slots__ = ("bar_types", "count", "end", "ids", "start")

    def __init__(self) -> None:
        self.ids: set[str] = set()
        self.bar_types: set[str] = set()
        self.start: int | None = None
        self.end: int | None = None
        self.count = 0

    def add(self, x: Any) -> None:
        bar_type = getattr(x, "bar_type", None)
        if bar_type is not None:
            self.bar_types.add(str(bar_type))
            self.ids.add(str(bar_type.instrument_id))
        elif hasattr(x, "instrument_id"):
            self.ids.add(str(x.instrument_id))
        if self.start is None:
            self.start = x.ts_init
        self.end = x.ts_init
        self.count += 1


def cached(
    root: str | os.PathLike[str],
    key: str,
    build: Callable[[], Feed],
    *,
    instruments: Sequence[Any] | Callable[[], Sequence[Any]] = (),
    rows_per_file: int = 1_000_000,
) -> list[CatalogFeed]:
    """The catalog feeds for `key` under `root`, written with `build()` and `instruments` only the
    first time any run asks for them.

    Each key is a catalog of its own (`root/<key>-<hash>/`), so keys never mix. It is used only once
    written whole: a write that failed or was killed part-way is written again by the next run, and
    runs asking at once wait for the one writing. A key names the data's reads, so change it when
    they change."""
    base = Path(root).resolve()
    base.mkdir(parents=True, exist_ok=True)
    target = base / _dirname(key)
    meta = target / META
    with _locked(base / f".{target.name}.lock"):
        if meta.exists():
            return [CatalogFeed(**d) for d in json.loads(meta.read_text())["feeds"]]
        if target.exists():
            shutil.rmtree(target)  # what a run that did not finish left
        insts = instruments() if callable(instruments) else instruments
        feeds = write(target, build(), instruments=insts, rows_per_file=rows_per_file, key=key)
        tmp = target / f"{META}.tmp"
        tmp.write_text(json.dumps({"key": key, "feeds": [asdict(f) for f in feeds]}, indent=2, sort_keys=True))
        tmp.replace(meta)
        return feeds


def _dirname(key: str) -> str:
    slug = re.sub(r"[^A-Za-z0-9._-]+", "-", key).strip("-.")[:64] or "feed"
    return f"{slug}-{hashlib.blake2b(key.encode(), digest_size=6).hexdigest()}"


@contextlib.contextmanager
def _locked(path: Path) -> Iterator[None]:
    with path.open("a") as f:
        fcntl.flock(f, fcntl.LOCK_EX)
        try:
            yield
        finally:
            fcntl.flock(f, fcntl.LOCK_UN)
