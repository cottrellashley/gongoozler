"""Running Nautilus backtests with progress, and keeping their results (spec/feeds.md,
spec/results.md). Importing this module needs nautilus_trader.

Two ways to run, one for each way data arrives:
- `run(engine, feeds)`: feeds of Python objects, streamed through Nautilus's own streaming loop
  (`add_data`, `run(streaming=True)` and `clear_data` per window, then `end()`). Progress is read
  between windows, with nothing inside the engine's loop: measured free. Memory is bounded by a
  window.
- `run_node(venues=..., data=..., strategies=...)`: data in Nautilus's Parquet catalogs, streamed by
  `BacktestNode` in Rust with no Python object per item: about 2x cheaper per run than objects
  built in Python (kb/measurements.md). Progress comes from an actor's timer, about a thousand
  times per run: measured within noise.

Both keep the same results: orders, fills, positions, account states, returns, statistics, a
series of balances and P&L sampled on a simulated-time grid, and a manifest of what produced them.
The engine is built with Nautilus's full API, as usual; only the data goes through here.
"""

from __future__ import annotations

import time
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from nautilus_trader.common import DataActor, DataActorConfig

from . import _events as ev
from . import results
from .feeds import Check, Feed, windows

UNSTABLE = frozenset({"init_id", "event_id", "causation_id", "events", "replay_events", "info"})
"""Fields of Nautilus's records that differ between identical runs: left out of results."""


@dataclass(frozen=True, slots=True)
class Summary:
    """What a streamed run went through: windows, items, the simulated span, and wall time."""

    windows: int
    items: int
    first: int | None
    last: int | None
    seconds: float


def run(
    engine: Any,
    feeds: Feed | Sequence[Feed],
    *,
    check: Check = "full",
    every_s: float = 0.25,
    name: str = "simulation",
    snapshot: bool = True,
    series_points: int = 1000,
    keep: bool = True,
) -> Summary:
    """Streams `feeds` into `engine` (a `BacktestEngine` with its venues, instruments and
    strategies added), ends the run, and keeps its results.

    - Feeds are added in the order given: on equal timestamps the earlier feed's data comes first,
      as in Nautilus. Declared instruments are checked against the engine before anything runs.
    - Progress is reported at most every `every_s` seconds of wall time, against the feeds' start
      and end when every feed declares an end, otherwise in items and simulated time. With
      `snapshot`, each report sends each venue's balances and P&L as live metrics.
    - With `keep`, results are written as artifacts (spec/results.md). When the feeds declare a start
      and an end, the series has `series_points` rows on an exact simulated-time grid.
    - If anything fails, the engine is still ended, the results so far are kept, and the error is
      raised.
    """
    fs = [feeds] if isinstance(feeds, Feed) else list(feeds)
    if not fs:
        raise ValueError("no feeds to run")
    _check_instruments(engine, fs)
    starts = [f.start for f in fs if f.start is not None]
    ends = [f.end for f in fs if f.end is not None]
    start = min(starts) if starts else None
    end = max(ends) if len(ends) == len(fs) else None
    recorder = None
    if keep and start is not None and end is not None and end > start:
        recorder = _attach_recorder(engine.add_actor, engine.list_venues(), engine.portfolio, start, end, series_points)
    ready = ev.throttled(every_s)
    snap = _Snapshot(engine) if snapshot else None
    n_windows = n_items = 0
    first = last = None
    t0 = time.monotonic()

    def kept(complete: bool) -> None:
        if keep:
            _keep(
                engine.cache,
                engine.list_venues(),
                engine.get_result(),
                recorder,
                feeds=[f.describe() for f in fs],
                run={"windows": n_windows, "items": n_items, "first": first, "last": last, "complete": complete},
            )

    try:
        with ev.stage(name):
            try:
                for w in windows(fs, check=check):
                    for _, items in w.parts:
                        engine.add_data(items if isinstance(items, list) else list(items))
                    engine.run(streaming=True)
                    engine.clear_data()
                    n_windows += 1
                    n_items += w.count
                    if first is None:
                        first = w.first
                    last = w.last
                    if ready():
                        _report(name, start if start is not None else first, end, last, n_items, time.monotonic() - t0, snap, final=False)
            finally:
                engine.end()
            _report(name, start if start is not None else first, end, last, n_items, time.monotonic() - t0, snap, final=True)
    except BaseException:
        _keep_quietly(kept)
        raise
    kept(True)
    return Summary(n_windows, n_items, first, last, time.monotonic() - t0)


def _keep_quietly(kept: Callable[[bool], None]) -> None:
    """Keeps what a failed run produced, without hiding the failure behind a second one."""
    try:
        kept(False)
    except Exception as err:  # noqa: BLE001 - the run's own error is the one to raise
        ev.log(f"results of the failed run could not be kept: {type(err).__name__}: {err}", "warning")


def _check_instruments(engine: Any, feeds: Sequence[Feed]) -> None:
    from nautilus_trader.model import InstrumentId

    missing = []
    for f in feeds:
        for iid in f.instruments or ():
            if engine.cache.instrument(InstrumentId.from_str(str(iid))) is None:
                missing.append(f"{iid} (feed {f.name!r})")
    if missing:
        raise ValueError(f"the engine has no instrument {', '.join(missing)}: add_instrument it before running")


def _report(name: str, start: int | None, end: int | None, sim: int | None, items: int, elapsed: float, snap: _Snapshot | None, *, final: bool) -> None:
    rate = items / elapsed if elapsed > 0 else None
    if start is not None and end is not None and end > start:
        total = end - start
        done = total if final else min(max((sim if sim is not None else start) - start, 0), total)
        frac = done / total
        eta = 0.0 if frac >= 1 else (elapsed * (1 - frac) / frac if frac > 0 else None)
        ev.progress(name, done, total, unit="ns", sim=sim, rate=rate, eta=eta)
    else:
        ev.progress(name, items, unit="items", sim=sim, rate=rate)
    if snap is not None and sim is not None:
        snap(sim)


class _Snapshot:
    """Live metrics read from the engine between windows, while it is paused. If the engine cannot
    answer, it says so once and stops asking."""

    def __init__(self, engine: Any) -> None:
        self.engine = engine
        self.on = True

    def __call__(self, at: int) -> None:
        if not self.on:
            return
        e = self.engine
        try:
            for name, value in _balances(e.cache, e.portfolio, e.list_venues()).items():
                ev.metric(name, value, at=at)
            ev.metric("positions.open", e.cache.positions_open_count(), at=at)
            ev.metric("orders", e.cache.orders_total_count(), at=at)
        except Exception as err:  # noqa: BLE001 - a snapshot must never stop a run
            self.on = False
            ev.log(f"portfolio snapshots stopped: {type(err).__name__}: {err}", "warning")


def _balances(cache: Any, portfolio: Any, venues: Sequence[Any]) -> dict[str, float]:
    out: dict[str, float] = {}
    for venue in venues:
        account = cache.account_for_venue(venue)
        if account is not None:
            for ccy, money in account.balances_total().items():
                out[f"balance.{venue}.{ccy}"] = money.as_double()
        for ccy, money in (portfolio.total_pnls(venue) or {}).items():
            out[f"pnl.{venue}.{ccy}"] = money.as_double()
    return out


# ---------------------------------------------------------------------------------- actors


class _Recorder(DataActor):  # type: ignore[misc]  # Nautilus ships no types
    """Samples balances, P&L and counts on an exact simulated-time grid: the series a run keeps,
    the same however its data was batched and however fast the machine. Nautilus 2.0 actors take
    one constructor argument."""

    def __init__(self, settings: tuple[Any, ...]) -> None:
        super().__init__(DataActorConfig(log_events=False, log_commands=False))
        self.start, self.every, self.venues, self.portfolio, self.rows = settings

    def on_start(self) -> None:
        self.clock.set_timer_ns("gongoozler.series", self.every, start_time_ns=self.start, callback=None, fire_immediately=True)

    def on_time_event(self, event: Any) -> None:
        if event.name != "gongoozler.series":
            return
        row: dict[str, Any] = {"ts": int(event.ts_event)}
        row.update(_balances(self.cache, self.portfolio, self.venues))
        row["positions_open"] = self.cache.positions_open_count()
        row["orders"] = self.cache.orders_total_count()
        self.rows.append(row)


def _attach_recorder(add_actor: Callable[[Any], None], venues: Sequence[Any], portfolio: Any, start: int, end: int, points: int) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    every = max((end - start) // max(points, 1), 1)
    add_actor(_Recorder((start, every, list(venues), portfolio, rows)))
    return rows


class _Progress(DataActor):  # type: ignore[misc]  # Nautilus ships no types
    """Reports simulated-time progress from inside the engine's loop, on a timer set to about
    `updates` events a run whatever its length: the cost measured within noise."""

    def __init__(self, settings: tuple[Any, ...]) -> None:
        super().__init__(DataActorConfig(log_events=False, log_commands=False))
        self.key, self.start, self.end, self.every = settings
        self.t0 = time.monotonic()

    def on_start(self) -> None:
        self.t0 = time.monotonic()
        self.clock.set_timer_ns("gongoozler.progress", self.every, start_time_ns=self.start + self.every, callback=None)

    def on_time_event(self, event: Any) -> None:
        if event.name != "gongoozler.progress":
            return
        now = int(event.ts_event)
        total = self.end - self.start
        done = min(max(now - self.start, 0), total)
        elapsed = time.monotonic() - self.t0
        frac = done / total if total else 0.0
        eta = elapsed * (1 - frac) / frac if 0 < frac < 1 else None
        ev.progress(self.key, done, total, unit="ns", sim=now, eta=eta)


# --------------------------------------------------------------------------------- the node


def run_node(
    *,
    venues: Sequence[Any],
    data: Sequence[Any],
    strategies: Sequence[Any],
    engine_config: Any = None,
    chunk_size: int = 100_000,
    name: str = "simulation",
    updates: int = 1000,
    series_points: int = 1000,
    start: int | None = None,
    end: int | None = None,
    keep: bool = True,
) -> Any:
    """Runs a `BacktestNode` over catalog data, with progress, and keeps its results. Returns its
    `BacktestResult`.

    - `venues`: `BacktestVenueConfig`s.
    - `data`: `CatalogFeed`s (`gongoozler.catalog`) or `BacktestDataConfig`s.
    - `strategies`: each one of:
      - `(type_name, config)` for one of Nautilus's built-in strategies;
      - an `ImportableStrategyConfig`;
      - a function of `(node, run_config_id)` that adds strategies itself.
    - The window for progress and the series comes from the catalog feeds, or from `start` and
      `end`.
    """
    from nautilus_trader.backtest import BacktestEngineConfig, BacktestNode, BacktestRunConfig
    from nautilus_trader.common import LoggerConfig
    from nautilus_trader.model import TraderId, Venue
    from nautilus_trader.trading import ImportableStrategyConfig

    from .catalog import CatalogFeed

    configs, described = [], []
    starts, ends = [], []
    for d in data:
        if isinstance(d, CatalogFeed):
            configs.append(d.data_config())
            described.append(d.describe())
            if d.start is not None:
                starts.append(d.start)
            if d.end is not None:
                ends.append(d.end)
        else:
            configs.append(d)
            described.append({"data_config": str(d)})
    start = start if start is not None else (min(starts) if starts else None)
    end = end if end is not None else (max(ends) if ends else None)
    engine_config = engine_config or BacktestEngineConfig(trader_id=TraderId("GONGOOZLER-001"), logging=LoggerConfig(bypass_logging=True))
    rc = BacktestRunConfig(venues=list(venues), data=configs, engine=engine_config, chunk_size=chunk_size, dispose_on_completion=False)
    node = BacktestNode([rc])
    node.build()
    for s in strategies:
        if callable(s):
            s(node, rc.id)
        elif isinstance(s, ImportableStrategyConfig):
            node.add_strategy_from_config(rc.id, s)
        else:
            type_name, config = s
            node.add_builtin_strategy(rc.id, type_name, config)
    recorder = None
    venue_ids = [Venue(v.name) for v in venues]
    portfolio = node.get_engine_portfolio(rc.id)
    if start is not None and end is not None and end > start:
        node.add_actor(rc.id, _Progress((name, start, end, max((end - start) // max(updates, 1), 1))))
        if keep:
            recorder = _attach_recorder(lambda a: node.add_actor(rc.id, a), venue_ids, portfolio, start, end, series_points)
    result = None

    def kept(complete: bool) -> None:
        if keep:
            _keep(node.get_engine_cache(rc.id), venue_ids, result, recorder, feeds=described, run={"chunk_size": chunk_size, "complete": complete})

    try:
        try:
            with ev.stage(name):
                out = node.run()
                result = out[0] if out else None
                if start is not None and end is not None and end > start:
                    ev.progress(name, end - start, end - start, unit="ns", sim=end, eta=0.0)
        except BaseException:
            _keep_quietly(kept)
            raise
        kept(True)
    finally:
        node.dispose()
    return result


# ---------------------------------------------------------------------------------- results


def _stable(d: dict[str, Any]) -> dict[str, Any]:
    return {k: v for k, v in d.items() if k not in UNSTABLE}


def _events_of(obj: Any) -> list[Any]:
    e = obj.events
    return list(e() if callable(e) else e)


def _keep(cache: Any, venues: Sequence[Any], result: Any, recorder: list[dict[str, Any]] | None, **inputs: Any) -> None:
    """Writes a run's results and manifest as artifacts (spec/results.md)."""
    orders = cache.orders()
    results.write_jsonl("orders.jsonl", (_stable(o.to_dict()) for o in orders))
    results.write_jsonl("fills.jsonl", (_stable(f.to_dict()) for o in orders for f in _events_of(o) if type(f).__name__ == "OrderFilled"))
    results.write_jsonl("positions.jsonl", (_stable(p.to_dict()) for p in cache.positions()))
    states: list[dict[str, Any]] = []
    for venue in venues:
        account = cache.account_for_venue(venue)
        if account is not None:
            states.extend(_stable(s.to_dict()) for s in _events_of(account))
    results.write_jsonl("account.jsonl", states)
    if result is not None:
        returns = sorted((int(ts), r) for ts, r in result.returns_series.items())
        results.write_jsonl("returns.jsonl", ({"ts": ts, "return": r} for ts, r in returns))
        results.write_json(
            "stats.json",
            {
                "pnls": result.stats_pnls,
                "returns": result.stats_returns,
                "general": result.stats_general,
                "orders": result.total_orders,
                "positions": result.total_positions,
            },
        )
    if recorder is not None:
        results.write_jsonl("series.jsonl", recorder)
    results.write_json("manifest.json", results.manifest(**inputs))


def tearsheet(engine: Any, path: str | Path = "tearsheet.html", **kwargs: Any) -> Path | None:
    """Writes Nautilus's own tearsheet for the run (`nautilus_trader.analysis`), and declares it as
    an artifact. It needs Nautilus's `visualization` extra; without it, this logs a warning and
    returns None."""
    try:
        from nautilus_trader.analysis.tearsheet import create_tearsheet
    except ImportError as err:
        ev.log(f"no tearsheet: {err} (it needs nautilus_trader[visualization])", "warning")
        return None
    target = ev.artifacts_dir() / Path(path).name if not Path(path).is_absolute() else Path(path)
    create_tearsheet(engine=engine, output_path=str(target), **kwargs)
    return ev.artifact(target)
