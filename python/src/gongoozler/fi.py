"""The fixed income engine under the watcher: its runs' results kept as artifacts (spec/results.md).

`fi_engine` reports its own progress. Under `gongoozler run`, its Rust loops (unit records,
ledgers, sweeps) write stages and progress into a ring of their own (spec/ring.md), with nothing
to set up in the file. What this module adds is the record of a run: its accounts, orders,
journal, holdings and statistics as canonical JSON, and a manifest of what produced them (the
engine's own manifest: series versions and provenance, knowledge policy, models, calendar, the
book's settings and step order, the mode, approximations). So `gongoozler diff` compares two runs
and `gongoozler rerun` proves one, byte for byte.

    import fi_engine as fi
    import gongoozler.fi as gzfi

    run = gzfi.run(ws, book, records)            # runs the book; keeps ledger.jsonl, stats.json, …
    runs = gzfi.sweep(ws, books, records)        # each book's results, prefixed by its name

It needs fi_engine: `gongoozler[fi]`.
"""

from __future__ import annotations

from collections.abc import Iterator, Sequence
from typing import Any

from . import _events as ev
from . import results

#: What a kept run writes, each prefixed by the book's name in a sweep.
FILES = ("ledger.jsonl", "orders.jsonl", "journal.jsonl", "holdings.json", "stats.json", "manifest.json")


def _rows(frame: Any) -> Iterator[dict[str, Any]]:
    """A Polars frame's rows, with times as integer nanoseconds and dates as ISO strings: exact,
    and the same bytes on every machine."""
    import polars as pl

    exprs = []
    for name, dtype in frame.schema.items():
        if isinstance(dtype, pl.Datetime):
            exprs.append(pl.col(name).dt.epoch("ns"))
        elif dtype == pl.Date:
            exprs.append(pl.col(name).dt.to_string("%Y-%m-%d"))
    if exprs:
        frame = frame.with_columns(exprs)
    yield from frame.iter_rows(named=True)


def _summary(run: Any) -> dict[str, Any]:
    failed = run.failed_at
    return {
        "name": run.name,
        "mode": run.mode,
        "final_nav": run.final_nav,
        "total_return": run.total_return,
        "trades": run.trades,
        "skipped_trades": run.skipped_trades,
        "unfilled_orders": run.unfilled_orders,
        "pending_at_end": run.pending_at_end,
        "cash_limited": run.cash_limited,
        "short_limited": run.short_limited,
        "shortfall": run.shortfall,
        "failed_at": None if failed is None else str(failed),
        "liquidation_nav": run.liquidation_nav,
        "max_identity_error": run.max_identity_error,
        "breaches": run.breaches,
        "output_version": f"{run.version:016x}",
    }


def keep(ws: Any, run: Any, book: Any, records: Any, *, prefix: str = "") -> list[str]:
    """Writes a run's results as artifacts, each name with `prefix`; returns the names."""
    import fi_engine

    names = []

    def jsonl(name: str, frame: Any) -> None:
        results.write_jsonl(prefix + name, _rows(frame))
        names.append(prefix + name)

    def json(name: str, obj: Any) -> None:
        results.write_json(prefix + name, obj)
        names.append(prefix + name)

    jsonl("ledger.jsonl", run.ledger())
    jsonl("orders.jsonl", run.orders())
    jsonl("journal.jsonl", run.journal())
    json("holdings.json", {"holdings": run.final_holdings, "cash": run.final_cash})
    json("stats.json", {"run": _summary(run), "performance": run.report(), "orders": run.order_stats})
    m = results.manifest(engine=ws.manifest(run, book, records).as_dict())
    m["packages"]["fi-engine"] = fi_engine.__version__
    json("manifest.json", m)
    return names


def run(ws: Any, book: Any, records: Any, *, keep_results: bool = True) -> Any:
    """Runs `book` on `records` (`Workspace.run`), keeps its results, and returns the run."""
    with ev.stage(f"fi.run {book.name}"):
        out = ws.run(book, records)
        if keep_results:
            keep(ws, out, book, records)
    return out


def sweep(ws: Any, books: Sequence[Any], records: Any, *, keep_results: bool = True) -> list[Any]:
    """Runs `books` in parallel (`Workspace.sweep`) and keeps each one's results, prefixed by its
    name. Two books of one name are refused: their results would overwrite each other."""
    names = [b.name for b in books]
    if len(set(names)) != len(names):
        dup = sorted({n for n in names if names.count(n) > 1})
        raise ValueError(f"books in a sweep need different names to keep their results apart: {dup}")
    with ev.stage(f"fi.sweep {len(books)} books"):
        out: list[Any] = list(ws.sweep(list(books), records))
        if keep_results:
            for b, r in zip(books, out, strict=True):
                keep(ws, r, b, records, prefix=f"{b.name}.")
    return out
