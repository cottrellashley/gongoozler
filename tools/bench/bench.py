"""The budgets (review/2026-11-03.md, "Budgets"), measured: `make bench` fails when one is missed.

- telemetry: a backtest run under the supervisor (events into a ring read from outside, progress
  and portfolio snapshots every 250 ms) against the same backtest with events off. Each side's
  best of several runs, alternated: at most 1% slower.
- emit: one progress update (a slot write) and one ordered event (a ring record), in this process.
- catalog ingest: BacktestNode streaming a catalog of quotes, with no strategy: at least a million
  quotes a second on its one core. With Nautilus's EmaCross too, for the record.

The Go side's budgets run in `make check`: event latency p99 under 250 ms
(TestEventsReachTheLogWithinAQuarterSecond), a run dead within 1 s of its supervisor and marked
lost (TestARunDiesWithinASecondOfItsSupervisor), and memory exact by cgroup or PSS
(TestMemorySharedByForkedWorkersIsCountedOnce).

Run: `make bench` (it builds the CLI first), or `python tools/bench/bench.py [--quick]` with
nautilus_trader installed and bin/gongoozler built.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CHILD = Path(__file__).with_name("child.py")
SEC = 1_000_000_000
BASE = 1_546_383_600 * SEC


def telemetry(n: int, runs: int) -> dict[str, float]:
    cli = ROOT / "bin" / "gongoozler"
    if not cli.exists():
        sys.exit("bench: build the CLI first (make build)")
    env = {**os.environ, "PYTHONPATH": str(ROOT / "python" / "src")}
    off, on = [], []
    with tempfile.TemporaryDirectory(prefix="gz-bench-") as tmp:
        state = Path(tmp) / "state"
        for i in range(runs):
            out = Path(tmp) / f"t{i}"
            subprocess.run([sys.executable, str(CHILD), str(out) + ".off", str(n)], env={**env, "GONGOOZLER_EVENTS": "off"}, check=True)
            off.append(float(Path(str(out) + ".off").read_text()))
            subprocess.run(
                [str(cli), "run", "--json", "--no-lock", "--cgroup", "off", "--launcher", sys.executable, str(CHILD), str(out) + ".on", str(n)],
                env={**env, "GONGOOZLER_STATE": str(state)},
                check=True,
                stdout=subprocess.DEVNULL,
            )
            on.append(float(Path(str(out) + ".on").read_text()))
    return {"off": min(off), "on": min(on), "overhead": min(on) / min(off) - 1}


def emit_costs(count: int = 200_000) -> dict[str, float]:
    sys.path.insert(0, str(ROOT / "python" / "src"))
    from gongoozler import _events, _ring

    with tempfile.TemporaryDirectory(prefix="gz-bench-") as tmp:
        transport = _events.RingTransport(_ring.Ring(Path(tmp), data_size=64 << 20))
        with _events._capture(transport):
            t0 = time.perf_counter()
            for i in range(count):
                _events.progress("bench", i, count)
            progress = (time.perf_counter() - t0) / count
            burst = 10_000  # within the rate limit's burst, so every one is written
            t0 = time.perf_counter()
            for i in range(burst):
                _events.emit("bench.tick", i=i)
            ordered = (time.perf_counter() - t0) / burst
    return {"progress": progress, "ordered": ordered}


def catalog_ingest(n: int) -> dict[str, float]:
    sys.path.insert(0, str(ROOT / "python" / "src"))
    from nautilus_trader.backtest import BacktestVenueConfig
    from nautilus_trader.execution import FixedFeeModel
    from nautilus_trader.model import AccountType, BookType, Currency, InstrumentId, Money, OmsType, Price, Quantity, QuoteTick, Symbol, Venue
    from nautilus_trader.testkit.providers import TestInstrumentProvider
    from nautilus_trader.trading import EmaCrossConfig

    import gongoozler as gz
    from gongoozler import catalog
    from gongoozler import nautilus as gzn

    usd = Currency.from_str("USD")
    venue = Venue("SIM")
    iid = InstrumentId(Symbol("AUD/USD"), venue)
    inst = TestInstrumentProvider.default_fx_ccy("AUD/USD", venue)

    def batches():  # built a batch at a time, so the whole feed is never held
        x = 0.71
        for start in range(0, n, 100_000):
            out = []
            for i in range(start, min(start + 100_000, n)):
                x += 0.00002 * math.sin(i / 97.0)
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
            yield out

    sim = BacktestVenueConfig(
        name="SIM",
        oms_type=OmsType.NETTING,
        account_type=AccountType.MARGIN,
        starting_balances=["1_000_000 USD"],
        book_type=BookType.L1_MBP,
        base_currency=usd,
        fee_model=FixedFeeModel(Money(0, usd)),
    )
    out: dict[str, float] = {}
    with tempfile.TemporaryDirectory(prefix="gz-bench-cat-") as tmp:
        os.environ["GONGOOZLER_EVENTS"] = "off"
        feeds = catalog.write(tmp, gz.feed("quotes", batches), instruments=[inst], rows_per_file=250_000)
        ema = ("EmaCross", EmaCrossConfig(instrument_id=inst.id, trade_size=Quantity.from_int(100_000), fast_period=10, slow_period=50))
        for name, strategies in (("ingest", []), ("ema_cross", [ema])):
            best = math.inf
            for _ in range(3):
                t0 = time.perf_counter()
                gzn.run_node(venues=[sim], data=feeds, strategies=strategies, keep=False)
                best = min(best, time.perf_counter() - t0)
            out[name] = n / best
    return out


BUDGETS = {
    "telemetry overhead": (lambda r: r["telemetry"]["overhead"], 0.01, "≤", lambda v: f"{v * 100:+.2f}%"),
    "progress update": (lambda r: r["emit"]["progress"], 5e-6, "≤", lambda v: f"{v * 1e6:.2f} µs"),
    "ordered event": (lambda r: r["emit"]["ordered"], 25e-6, "≤", lambda v: f"{v * 1e6:.2f} µs"),
    "catalog ingest": (lambda r: r["catalog"]["ingest"], 1e6, "≥", lambda v: f"{v / 1e6:.2f}M quotes/s"),
}


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--quick", action="store_true", help="smaller runs, for a smoke test")
    ap.add_argument("--json", action="store_true", help="print the measurements as JSON")
    a = ap.parse_args()
    n, runs, cat = (100_000, 3, 300_000) if a.quick else (300_000, 5, 1_000_000)
    r = {"emit": emit_costs(), "telemetry": telemetry(n, runs), "catalog": catalog_ingest(cat)}
    if a.json:
        print(json.dumps(r, indent=2))
    failed = 0
    print(f"{'budget':22} {'measured':>18} {'limit':>18}")
    for name, (get, limit, op, fmt) in BUDGETS.items():
        v = get(r)
        ok = v <= limit if op == "≤" else v >= limit
        failed += not ok
        print(f"{name:22} {fmt(v):>18} {op + ' ' + fmt(limit):>18}  {'ok' if ok else 'MISSED'}")
    t = r["telemetry"]
    print(f"(telemetry: {t['off']:.3f}s with events off, {t['on']:.3f}s supervised; EmaCross over a catalog: {r['catalog']['ema_cross'] / 1e6:.2f}M quotes/s)")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
