"""Results are data (spec/results.md): the same run writes the same bytes in any process, however
its data was batched, on an exact simulated-time grid; a failed run keeps what it had."""

from __future__ import annotations

import hashlib
import json
import os
import subprocess
import sys
from pathlib import Path

import pytest

pytest.importorskip("nautilus_trader")

import gongoozler as gz
from gongoozler import nautilus as gzn

from .test_nautilus import BASE, SEC, engine, quotes

ROOT = Path(__file__).resolve().parents[1]
RESULTS = ("orders.jsonl", "fills.jsonl", "positions.jsonl", "account.jsonl", "returns.jsonl", "stats.json", "series.jsonl")
SCRIPT = """
import sys
sys.path.insert(0, {root!r})
from tests.test_nautilus import quotes, engine, BASE, SEC
import gongoozler as gz
from gongoozler import nautilus as gzn
data = quotes("AUD/USD", [BASE + i * SEC for i in range(20_000)])
e, _ = engine(["AUD/USD"])
gzn.run(e, gz.from_items("quotes", data, batch_size={batch}, start=BASE, end=data[-1].ts_init, kind="QuoteTick", instruments=["AUD/USD.SIM"]),
        snapshot=False, series_points=200)
e.dispose()
"""


def run_elsewhere(out: Path, batch: int) -> dict[str, str]:
    """Runs the backtest in a fresh interpreter and returns each file it kept, by hash."""
    env = {**os.environ, "PYTHONPATH": str(ROOT / "src"), "GONGOOZLER_ARTIFACTS": str(out), "GONGOOZLER_EVENTS": "off"}
    subprocess.run([sys.executable, "-c", SCRIPT.format(root=str(ROOT), batch=batch)], env=env, check=True, timeout=120, capture_output=True)
    return {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted(out.iterdir())}


def test_the_same_run_in_another_process_writes_the_same_bytes(tmp_path: Path) -> None:
    a = run_elsewhere(tmp_path / "a", 1_000)
    b = run_elsewhere(tmp_path / "b", 1_000)
    assert set(RESULTS) | {"manifest.json"} <= set(a)
    assert a == b
    fills = (tmp_path / "a" / "fills.jsonl").read_text().splitlines()
    assert len(fills) > 50 and all("trade_id" in json.loads(f) for f in fills)


def test_results_do_not_depend_on_how_the_data_was_batched(tmp_path: Path) -> None:
    a = run_elsewhere(tmp_path / "a", 1_000)
    b = run_elsewhere(tmp_path / "b", 7_919)
    assert {k: a[k] for k in RESULTS} == {k: b[k] for k in RESULTS}
    ma, mb = (json.loads((tmp_path / d / "manifest.json").read_text()) for d in "ab")
    assert ma["inputs"]["feeds"] == mb["inputs"]["feeds"], "the same data, fingerprinted the same"
    assert (ma["inputs"]["run"]["windows"], mb["inputs"]["run"]["windows"]) == (20, 3)


def test_the_series_lies_on_an_exact_simulated_time_grid(artifacts: Path) -> None:
    data = quotes("AUD/USD", [BASE + i * SEC for i in range(10_000)])
    end = data[-1].ts_init
    e, _ = engine(["AUD/USD"])
    gzn.run(e, gz.from_items("quotes", data, batch_size=777, start=BASE, end=end), snapshot=False, series_points=100)
    e.dispose()
    rows = [json.loads(x) for x in (artifacts / "series.jsonl").read_text().splitlines()]
    every = (end - BASE) // 100
    assert [r["ts"] for r in rows] == [BASE + k * every for k in range(len(rows))]
    assert len(rows) in (100, 101) and rows[-1]["ts"] <= end
    assert rows[0]["balance.SIM.USD"] == 1_000_000.0 and {"pnl.SIM.USD", "positions_open", "orders"} <= set(rows[-1])
    assert rows[-1]["orders"] > 0


def test_the_manifest_says_what_produced_the_results(artifacts: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("GONGOOZLER_PARAMS", '{"fast": 12}')
    p = gz.params(fast=10, slow=50)
    data = quotes("AUD/USD", [BASE + i * SEC for i in range(2_000)])
    e, _ = engine(["AUD/USD"])
    gzn.run(e, gz.from_items("quotes", data, start=BASE, end=data[-1].ts_init, revision="2026-10-31"), snapshot=False)
    e.dispose()
    m = json.loads((artifacts / "manifest.json").read_text())
    assert m["params"] == {"fast": 12, "slow": 50} == vars(p)
    assert m["gongoozler"] == gz.__version__ and m["packages"]["nautilus_trader"] == "2.0.0rc8"
    (f,) = m["inputs"]["feeds"]
    assert (f["name"], f["revision"], f["fingerprint"]["count"]) == ("quotes", "2026-10-31", 2_000)
    assert m["inputs"]["run"]["complete"] is True and set(m["environment"]) == {"platform", "machine", "node"}


def test_a_run_that_fails_keeps_what_it_had_and_says_it_is_incomplete(artifacts: Path) -> None:
    data = quotes("AUD/USD", [BASE + i * SEC for i in range(6_000)])
    broken = gz.feed("quotes", [data[:2_000], data[2_000:4_000], data[:10]])
    e, _ = engine(["AUD/USD"])
    with pytest.raises(gz.FeedError, match="goes back in time"):
        gzn.run(e, broken, snapshot=False)
    e.dispose()
    m = json.loads((artifacts / "manifest.json").read_text())
    assert m["inputs"]["run"] == {"windows": 2, "items": 4_000, "first": BASE, "last": data[3_999].ts_init, "complete": False}
    assert (artifacts / "orders.jsonl").read_text().count("\n") > 0


def test_a_run_asked_not_to_keep_writes_nothing(artifacts: Path) -> None:
    data = quotes("AUD/USD", [BASE + i * SEC for i in range(1_000)])
    e, _ = engine(["AUD/USD"])
    gzn.run(e, gz.from_items("quotes", data, start=BASE, end=data[-1].ts_init), snapshot=False, keep=False)
    e.dispose()
    assert list(artifacts.iterdir()) == []


def test_an_instrument_a_feed_declares_must_be_in_the_engine() -> None:
    data = quotes("AUD/USD", [BASE + i * SEC for i in range(10)])
    e, _ = engine(["AUD/USD"])
    with pytest.raises(ValueError, match=r"the engine has no instrument EUR/USD.SIM \(feed 'quotes'\): add_instrument it"):
        gzn.run(e, gz.from_items("quotes", data, instruments=["AUD/USD.SIM", "EUR/USD.SIM"]))
    e.dispose()
