"""The fixed income engine under the watcher (review H6): its Rust loops report into a ring of their
own, read back here the spec's way; its results are kept as canonical artifacts, the same bytes
from the same run in any process."""

from __future__ import annotations

import hashlib
import json
import os
import subprocess
import sys
from pathlib import Path

import pytest

pytest.importorskip("fi_engine")

from gongoozler import fi as gzfi

from .ringread import rings

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = """
import fi_engine as fi
import gongoozler.fi as gzfi
ws = fi.Workspace()
u = ws.synthetic(bonds=120, days=400, seed={seed})
rec = ws.records(u["bonds"], u["calendar"])
books = [fi.Strategy(fi.Decision.top_n("marks.signal", n), fill=fi.Fill.research(2.0), journal=True, name=f"top-{{n}}") for n in (10, 20)]
gzfi.run(ws, books[0], rec)
gzfi.sweep(ws, books, rec)
"""


def run_elsewhere(out: Path, *, seed: int = 7, ring: Path | None = None) -> dict[str, str]:
    """Runs the engine in a fresh interpreter and returns each file it kept, by hash."""
    env = {**os.environ, "PYTHONPATH": str(ROOT / "src"), "GONGOOZLER_ARTIFACTS": str(out), "GONGOOZLER_EVENTS": "off"}
    env.pop("GONGOOZLER_RING", None)
    if ring is not None:
        env["GONGOOZLER_RING"] = str(ring)
        env["GONGOOZLER_RUN_ID"] = "fi-test"
    subprocess.run([sys.executable, "-c", SCRIPT.format(seed=seed)], env=env, check=True, timeout=300, capture_output=True)
    return {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted(out.iterdir())}


def test_an_engine_run_keeps_its_results_and_the_same_run_writes_the_same_bytes(tmp_path: Path) -> None:
    a = run_elsewhere(tmp_path / "a")
    b = run_elsewhere(tmp_path / "b")
    assert set(gzfi.FILES) <= set(a), "the run's own results"
    assert {f"top-{n}.{f}" for n in (10, 20) for f in gzfi.FILES} <= set(a), "each book of the sweep apart"
    assert a == b, "the same run in two processes: byte for byte"
    c = run_elsewhere(tmp_path / "c", seed=8)
    assert c["ledger.jsonl"] != a["ledger.jsonl"], "other data, other accounts"
    ledger = [json.loads(line) for line in (tmp_path / "a" / "ledger.jsonl").read_text().splitlines()]
    assert len(ledger) == 400 and isinstance(ledger[0]["time"], int), "times as exact nanoseconds"
    stats = json.loads((tmp_path / "a" / "stats.json").read_text())
    assert stats["run"]["max_identity_error"] < 1e-9 and stats["run"]["trades"] > 0
    manifest = json.loads((tmp_path / "a" / "manifest.json").read_text())
    assert manifest["packages"]["fi-engine"] and manifest["inputs"]["engine"]["fingerprint"]
    assert "StepOrder" in manifest["inputs"]["engine"]["settings"], "the step order is part of what produced it"
    journal = (tmp_path / "a" / "journal.jsonl").read_text().splitlines()
    assert any(json.loads(j)["kind"] == "fill" for j in journal)


def test_the_engine_reports_its_progress_into_a_ring_of_its_own(tmp_path: Path) -> None:
    ring = tmp_path / "ring"
    ring.mkdir()
    run_elsewhere(tmp_path / "out", ring=ring)
    readers = rings(str(ring))
    events = {r.path: r.drain() for r in readers}
    engine = [p for p, evs in events.items() if evs and evs[0]["kind"] == "hello" and evs[0].get("producer") == "fi-engine"]
    assert len(readers) == 2 and len(engine) == 1, "the Python emitter's ring and the engine's own"
    evs = events[engine[0]]
    assert all(e["v"] == 1 and e["run"] == "fi-test" for e in evs)
    assert [e["seq"] for e in evs] == sorted({e["seq"] for e in evs}), "strictly increasing"
    stages = [(e["name"], e["phase"]) for e in evs if e["kind"] == "stage"]
    assert stages[:2] == [("fi.records", "begin"), ("fi.records", "end")]
    assert ("fi.ledger", "begin") in stages and ("fi.sweep", "end") in stages
    assert all(e["ok"] for e in evs if e["kind"] == "stage" and e["phase"] == "end")
    reader = next(r for r in readers if r.path == engine[0])
    final = {s.key: (s.a, s.b) for s in reader.slots()}
    assert final["fi.records"] == (120.0, 120.0) and final["fi.ledger"] == (400.0, 400.0) and final["fi.sweep"] == (2.0, 2.0)
    assert reader.counters() == {"dropped": 0, "truncated": 0, "limited": 0, "closed": 1, "slots_full": 0}, "closed cleanly at exit"
    for r in readers:
        r.close()


def test_a_sweep_refuses_books_whose_results_would_overwrite_each_other() -> None:
    import fi_engine as fi

    ws = fi.Workspace()
    u = ws.synthetic(bonds=20, days=30)
    rec = ws.records(u["bonds"], u["calendar"])
    same = [fi.Strategy(fi.Decision.top_n("marks.signal", n), name="same") for n in (2, 3)]
    with pytest.raises(ValueError, match="different names"):
        gzfi.sweep(ws, same, rec, keep_results=False)
