"""The emitter's promises (spec/protocol.md, spec/ring.md): order, coalescing into slots, causal
order between slots and records, values that can never break the stream, sizes that are capped,
limits that are counted, and no thread, lock wait or I/O wait in the run."""

from __future__ import annotations

import io
import json
import threading
import time
from pathlib import Path

import pytest

import gongoozler as gz
from gongoozler import _events, _ring

from .conftest import Captured, run_script
from .ringread import rings


def test_stages_nest_and_records_arrive_in_order_with_increasing_sequence_numbers(captured: Captured) -> None:
    with gz.stage("outer"), gz.stage("inner"):
        gz.emit("custom", n=1)
    stages = [(e["name"], e["phase"], e["depth"]) for e in captured.of("stage")]
    assert stages == [("outer", "begin", 0), ("inner", "begin", 1), ("inner", "end", 1), ("outer", "end", 0)]
    assert captured.kinds().index("custom") == 2
    seqs = [e["seq"] for e in captured.events()]
    assert seqs == sorted(seqs) and len(set(seqs)) == len(seqs)
    assert all(e["v"] == 1 and isinstance(e["t"], float) for e in captured.events())


def test_a_failing_stage_reports_the_error_and_reraises(captured: Captured) -> None:
    with pytest.raises(KeyError), gz.stage("pull"):
        raise KeyError("isin")
    end = captured.of("stage")[-1]
    assert (end["phase"], end["ok"]) == ("end", False)
    assert "KeyError" in end["error"]


def test_progress_lives_in_a_slot_and_a_hundred_thousand_updates_write_no_records(captured: Captured) -> None:
    for i in range(100_000):
        gz.progress("sim", i, 100_000, sim=1_700_000_000 * 10**9 + i)
    assert captured.events() == []
    (slot,) = captured.reader.slots()
    assert (slot.kind, slot.key, slot.a, slot.b, slot.c) == (_ring.KIND_PROGRESS, "sim", 99_999.0, 100_000.0, 1_700_000_000 * 10**9 + 99_999)
    assert slot.flags & _ring.F_A_INT


def test_progress_made_before_an_ordered_event_is_written_ahead_of_it(captured: Captured) -> None:
    with gz.stage("s"):
        gz.progress("rows", 5, 10)
        gz.metric("nav", 101.5)
    kinds = captured.kinds()
    assert kinds == ["stage", "progress", "metric", "stage"]
    progress = captured.of("progress")[0]
    assert (progress["done"], progress["slot"]) == (5, 0) and progress["sseq"] > 0


def test_a_slot_value_carries_the_last_record_before_it(captured: Captured) -> None:
    gz.log("first")
    gz.progress("rows", 1)
    first = captured.of("log")[0]
    (slot,) = captured.reader.slots()
    assert slot.after == first["seq"], "a reader may show this value only after reading the log line"


def test_no_value_a_run_passes_can_break_the_stream(captured: Captured) -> None:
    class Broken:
        def __str__(self) -> str:
            raise RuntimeError("no text")

    gz.emit("custom", nan=float("nan"), inf=float("-inf"), keys={3: "int", (1, 2): "tuple"}, deep=[[[[[[[[[[1]]]]]]]]]], odd=Broken(), raw=b"\x00\xff")
    gz.metric("sharpe", float("nan"))
    gz.progress("p", float("inf"), 10.0)
    gz.log("still here")
    (custom,) = captured.of("custom")
    assert custom["nan"] is None and custom["inf"] is None
    assert custom["keys"] == {"3": "int", "(1, 2)": "tuple"}
    assert custom["odd"].startswith("<Broken: str() raised RuntimeError")
    assert captured.of("log")[0]["msg"] == "still here"
    slots = {s.key: s for s in captured.reader.slots()}
    assert slots["sharpe"].flags & _ring.F_A_NULL and slots["p"].flags & _ring.F_A_NULL


def test_huge_values_are_cut_to_fit_and_counted(captured: Captured) -> None:
    gz.log("x" * 5_000_000)
    gz.emit("big", rows=list(range(50_000)), blob="y" * 2_000_000)
    gz.log("after")
    msgs = [e["msg"] for e in captured.of("log")]
    assert len(msgs[0]) < 70_000 and msgs[0].endswith("more characters]")
    assert msgs[1] == "after"
    (big,) = captured.of("big")
    assert len(big["rows"]) == _events.MAX_ITEMS
    assert captured.counters()["truncated"] >= 3


def test_an_event_too_large_even_after_cutting_is_replaced_by_a_stub(captured: Captured) -> None:
    gz.emit("wide", **{f"f{i}": "z" * 60_000 for i in range(40)})
    (stub,) = captured.of("wide")
    assert stub["truncated"] is True and stub["bytes"] > _events.MAX_EVENT


def test_logs_and_other_kinds_over_their_limits_are_counted_not_lost_silently(captured: Captured) -> None:
    for i in range(5_000):
        gz.log(f"line {i}")
    for i in range(15_000):
        gz.emit("tick", i=i)
    with gz.stage("exempt"):
        pass
    sent_logs, sent_ticks = len(captured.of("log")), len(captured.of("tick"))
    assert sent_logs <= _events.LOG_BURST + 10 and sent_ticks <= _events.EVENT_BURST + 100
    assert captured.counters()["limited"] == (5_000 - sent_logs) + (15_000 - sent_ticks)
    assert len(captured.of("stage")) == 2


def test_a_full_ring_drops_counts_and_never_raises(tmp_path) -> None:  # type: ignore[no-untyped-def]
    t = _events.RingTransport(_ring.Ring(tmp_path, data_size=1 << 16))
    with _events._capture(t):
        for i in range(2_000):
            gz.emit("x", i=i, pad="p" * 100)
    (r,) = rings(str(tmp_path))
    c = r.counters()
    assert c["dropped"] > 0 and len(r.drain()) + c["dropped"] == 2_000
    assert c["closed"] == 1


def test_artifact_declares_the_file_without_reading_it(captured: Captured, tmp_path) -> None:  # type: ignore[no-untyped-def]
    f = tmp_path / "report.html"
    f.write_text("<html></html>")
    assert gz.artifact(f) == f
    (d,) = captured.of("artifact.declare")
    assert (d["name"], d["path"], d["bytes"]) == ("report.html", str(f), 13)
    assert "sha256" not in d, "the supervisor hashes it, out of the run's process"


def test_reserved_kinds_cannot_be_emitted() -> None:
    for kind in ("progress", "metric", "hello", "artifact.declare"):
        with pytest.raises(ValueError):
            gz.emit(kind)


def test_threads_share_one_ring_safely(captured: Captured) -> None:
    def work(k: int) -> None:
        for i in range(2_000):
            gz.progress(f"p{k}", i)
            if i % 100 == 0:
                gz.emit("mark", k=k, i=i)

    threads = [threading.Thread(target=work, args=(k,)) for k in range(8)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    assert len(captured.of("mark")) == 8 * 20
    assert {s.key: s.a for s in captured.reader.slots()} == {f"p{k}": 1_999.0 for k in range(8)}


def test_emitting_progress_costs_microseconds(captured: Captured) -> None:
    n = 100_000
    t = time.perf_counter()
    for i in range(n):
        gz.progress("sim", i, n)
    per_call = (time.perf_counter() - t) / n
    assert per_call < 10e-6, f"{per_call * 1e6:.2f} µs per call"


# ------------------------------------------------------------------- processes, under a supervisor


def test_each_process_writes_its_own_ring_and_a_crash_loses_nothing_written(tmp_path) -> None:  # type: ignore[no-untyped-def]
    code = (
        "import os, gongoozler as gz\n"
        "gz.log('parent')\n"
        "pid = os.fork()\n"
        "if pid == 0:\n"
        "    gz.log('child')\n"
        "    os._exit(0)\n"  # no clean close: the child's ring must still be readable
        "os.waitpid(pid, 0)\n"
    )
    r = run_script(code, {"GONGOOZLER_RING": str(tmp_path)})
    assert r.returncode == 0, r.stderr
    readers = rings(str(tmp_path))
    assert len(readers) == 2
    by = {rd.pid: rd for rd in readers}
    msgs = {pid: [e.get("msg") for e in rd.drain() if e["kind"] == "log"] for pid, rd in by.items()}
    assert sorted(m for ms in msgs.values() for m in ms) == ["child", "parent"]
    closed = sorted(rd.counters()["closed"] for rd in readers)
    assert closed == [0, 1], "the parent closed its ring at exit; the child, ended by os._exit, did not"


def test_an_uncaught_exception_in_the_main_thread_or_another_is_reported(tmp_path) -> None:  # type: ignore[no-untyped-def]
    code = (
        "import threading, gongoozler as gz\n"
        "gz.log('starting')\n"
        "t = threading.Thread(target=lambda: 1 / 0, name='worker'); t.start(); t.join()\n"
        "raise RuntimeError('margin call unmet')\n"
    )
    r = run_script(code, {"GONGOOZLER_RING": str(tmp_path)})
    assert r.returncode == 1 and "margin call unmet" in r.stderr
    (rd,) = rings(str(tmp_path))
    errors = [e for e in rd.drain() if e["kind"] == "error"]
    assert [(e["type"], e.get("thread")) for e in errors] == [("ZeroDivisionError", "worker"), ("RuntimeError", None)]
    assert errors[1]["where"].endswith(":4")


def test_hello_names_the_script_but_keeps_its_arguments_to_itself(tmp_path) -> None:  # type: ignore[no-untyped-def]
    r = run_script("import sys, gongoozler as gz; gz.log('x')", {"GONGOOZLER_RING": str(tmp_path)})
    assert r.returncode == 0, r.stderr
    (rd,) = rings(str(tmp_path))
    hello = rd.drain()[0]
    assert hello["kind"] == "hello" and "argv" not in hello and hello["argc"] == 0


# --------------------------------------------------------------------------- with no supervisor


def test_with_no_supervisor_progress_prints_to_the_terminal() -> None:
    out = io.StringIO()
    t = _events.TerminalTransport(out)
    with _events._capture(t):
        with gz.stage("simulate"):
            gz.progress("simulation", 50, 200, sim=1_700_000_000 * 10**9, rate=2.5e6)
        gz.log("done")
    text = out.getvalue()
    assert "▸ simulate" in text and "✓ simulate (" in text and "[info] done" in text
    assert "simulation   25.0%  2023-11-14T22:13:20Z  2.5M/s" in text


def test_the_file_transport_writes_every_ordered_event_and_the_last_progress(tmp_path) -> None:  # type: ignore[no-untyped-def]
    path = tmp_path / "events.jsonl"
    with _events._capture(_events.FileTransport(str(path))):
        for i in range(1_000):
            gz.progress("p", i)
        gz.log("end")
    lines = [json.loads(line) for line in path.read_text().splitlines()]
    kinds = [e["kind"] for e in lines]
    assert kinds.count("log") == 1 and kinds[-1] == "log"
    progress = [e for e in lines if e["kind"] == "progress"]
    assert progress[-1]["done"] == 999, "the last progress is written ahead of the log line that followed it"


def test_a_ring_that_cannot_be_made_falls_back_to_the_terminal() -> None:
    r = run_script("import gongoozler as gz\ngz.log('still runs')\n", {"GONGOOZLER_RING": "/nonexistent/ring"})
    assert r.returncode == 0
    assert "printing progress here" in r.stderr and "still runs" in r.stderr


def test_under_a_supervisor_prints_are_written_a_line_at_a_time(tmp_path: Path) -> None:
    code = "import sys, gongoozler; print(sys.stdout.line_buffering, sys.stdout.write_through, sys.stderr.line_buffering)"
    supervised = run_script(code, {"GONGOOZLER_RING": str(tmp_path), "PYTHONUNBUFFERED": "1"})
    assert supervised.stdout.split() == ["True", "False", "True"], supervised.stderr
    alone = run_script(code, {"PYTHONUNBUFFERED": "1"})
    assert alone.stdout.split()[1] == "True", "with no supervisor, the stream is left as it was"
