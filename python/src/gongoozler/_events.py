"""Progress events: what a run emits, and where the events go (spec/protocol.md, spec/ring.md).

Under a supervisor (the CLI, or a box), GONGOOZLER_RING names a directory, and this process writes
its events into a shared-memory ring there (`_ring.py`). The supervisor reads it from outside the
process. Nothing here runs in a background thread, and nothing waits:
- ordered events (hello, stage, log, artifact declarations, errors, params, anything sent through
  `emit`) become ring records, in order;
- `progress` and `metric` overwrite latest-value slots. Before an ordered event is written, the
  slots updated since the last one are written as records ahead of it, so progress is never read
  after the end of the stage that made it;
- every value is made JSON-safe (non-finite numbers become null; anything else becomes text), and
  sizes are capped, so no value a run passes can break the stream;
- logs and every other ordered kind are rate-limited.

Whatever is dropped, cut down or refused is counted in the ring's header, which the supervisor
reports. Nothing is lost silently.

With no supervisor, GONGOOZLER_EVENTS says where events go:
- `stderr`, the default: a plain view in the terminal;
- `file:PATH`: newline-delimited JSON;
- `off`: nowhere.
"""

from __future__ import annotations

import atexit
import contextlib
import contextvars
import json
import math
import os
import sys
import threading
import time
from collections.abc import Callable, Iterator
from pathlib import Path
from types import TracebackType
from typing import Any, ClassVar, Literal, TextIO

from . import _ring

PROTOCOL = 1
ENV_RING = "GONGOOZLER_RING"
ENV_EVENTS = "GONGOOZLER_EVENTS"
ENV_RUN = "GONGOOZLER_RUN_ID"
ENV_ARTIFACTS = "GONGOOZLER_ARTIFACTS"
ENV_ARGV = "GONGOOZLER_ARGV"

MAX_STR = 64 * 1024
"""Characters kept of any one string."""
MAX_ITEMS = 10_000
"""Items kept of any one list, and keys of any one dict."""
MAX_DEPTH = 8
MAX_EVENT = 1 << 20
"""Bytes of one encoded event; a larger one is replaced by a stub that says how large it was."""
LOG_RATE, LOG_BURST = 200.0, 2_000
EVENT_RATE, EVENT_BURST = 1_000.0, 10_000
"""The limit for each other ordered kind sent through `emit`."""
EXEMPT = frozenset({"hello", "stage", "error", "artifact.declare", "params", "progress.unit"})
"""Kinds a run makes rarely and a viewer needs whole: never rate-limited."""


# ------------------------------------------------------------------------------- JSON safety


class _Cuts:
    __slots__ = ("n",)

    def __init__(self) -> None:
        self.n = 0


def _cap(s: str, cuts: _Cuts) -> str:
    if len(s) <= MAX_STR:
        return s
    cuts.n += 1
    return s[:MAX_STR] + f"…[{len(s) - MAX_STR} more characters]"


def clean(v: Any, cuts: _Cuts | None = None, depth: int = 0) -> Any:
    """A JSON-safe copy of `v`: non-finite numbers become null, strings and containers are capped,
    keys become strings, and anything else becomes its text."""
    cuts = cuts if cuts is not None else _Cuts()
    if v is None or isinstance(v, bool | int):
        return v
    if isinstance(v, float):
        return v if math.isfinite(v) else None
    if isinstance(v, str):
        return _cap(v, cuts)
    if depth >= MAX_DEPTH:
        cuts.n += 1
        return "…"
    if isinstance(v, dict):
        if len(v) > MAX_ITEMS:
            cuts.n += 1
        out = {}
        for i, (k, x) in enumerate(v.items()):
            if i >= MAX_ITEMS:
                break
            out[_cap(k if isinstance(k, str) else str(k), cuts)] = clean(x, cuts, depth + 1)
        return out
    if isinstance(v, (list, tuple)):
        if len(v) > MAX_ITEMS:
            cuts.n += 1
        return [clean(x, cuts, depth + 1) for x in v[:MAX_ITEMS]]
    try:
        text = str(v)
    except Exception as err:  # noqa: BLE001 - a value whose str() raises still becomes text
        text = f"<{type(v).__name__}: str() raised {type(err).__name__}>"
    return _cap(text, cuts)


def encode(ev: dict[str, Any]) -> tuple[bytes, bool]:
    """One event as compact JSON, and whether it had to be replaced to fit `MAX_EVENT`."""
    try:
        b = json.dumps(ev, separators=(",", ":"), allow_nan=False, ensure_ascii=False).encode()
    except (TypeError, ValueError) as err:  # clean() makes this unreachable; kept as the last guard
        stub = {"kind": "telemetry", "t": ev.get("t"), "seq": ev.get("seq"), "v": PROTOCOL, "error": f"an event could not be encoded: {err}"}
        return json.dumps(stub).encode(), True
    if len(b) <= MAX_EVENT:
        return b, False
    stub = {"kind": ev.get("kind"), "t": ev.get("t"), "seq": ev.get("seq"), "v": PROTOCOL, "truncated": True, "bytes": len(b)}
    for k in ("run", "name", "key"):
        if k in ev:
            stub[k] = ev[k]
    return json.dumps(stub, separators=(",", ":"), ensure_ascii=False).encode(), True


# ------------------------------------------------------------------------------- transports


class Transport:
    """Where events go. Called by the emitter, under its lock."""

    def ordered(self, em: Emitter, ev: dict[str, Any], payload: bytes) -> None:
        raise NotImplementedError

    def latest(self, em: Emitter, kind: str, key: str, fields: dict[str, Any], t: float) -> None:
        raise NotImplementedError

    def before_ordered(self, em: Emitter) -> None:
        """Called before each ordered event: the ring writes pending slot values ahead of it."""

    def count(self, what: str, n: int = 1) -> None:
        raise NotImplementedError

    def close(self, em: Emitter) -> None:
        pass


class RingTransport(Transport):
    """The supervised path: a shared-memory ring (spec/ring.md)."""

    _COUNTERS: ClassVar[dict[str, int]] = {"dropped": _ring.O_DROPPED, "truncated": _ring.O_TRUNCATED, "limited": _ring.O_LIMITED}

    def __init__(self, ring: _ring.Ring) -> None:
        self.ring = ring
        self.keys: dict[str, bytes] = {}
        self.dirty: dict[int, tuple[str, dict[str, Any], float, int]] = {}

    def ordered(self, em: Emitter, ev: dict[str, Any], payload: bytes) -> None:
        self.ring.write(payload)

    def latest(self, em: Emitter, kind: str, key: str, fields: dict[str, Any], t: float) -> None:
        kid = _ring.KIND_PROGRESS if kind == "progress" else _ring.KIND_METRIC
        i = self.ring.slot(kid, key)
        if i is None:
            return
        kb = self.keys.get(key)
        if kb is None:
            kb = self.keys[key] = key.encode()[: _ring.SLOT_KEY_MAX]
        if kind == "progress":
            seq = self.ring.set(i, kid, kb, fields["done"], fields.get("total"), fields.get("sim"), fields.get("rate"), fields.get("eta"), t, em._seq)
        else:
            seq = self.ring.set(i, kid, kb, fields["value"], None, fields.get("at"), None, None, t, em._seq)
        self.dirty[i] = (kind, fields, t, seq)

    def before_ordered(self, em: Emitter) -> None:
        if not self.dirty:
            return
        dirty, self.dirty = self.dirty, {}
        for i, (kind, fields, t, sseq) in dirty.items():
            ev = {"kind": kind, "t": t, **fields, "seq": em.next_seq(), "v": PROTOCOL, "slot": i, "sseq": sseq}
            if em.run_id:
                ev["run"] = em.run_id
            self.ring.write(encode(ev)[0])

    def count(self, what: str, n: int = 1) -> None:
        self.ring.count(self._COUNTERS[what], n)

    def close(self, em: Emitter) -> None:
        self.ring.close()


class _Counting(Transport):
    """Shared by the transports with no ring: drops are counted in process and reported at close."""

    def __init__(self) -> None:
        self.counts: dict[str, int] = {}

    def count(self, what: str, n: int = 1) -> None:
        self.counts[what] = self.counts.get(what, 0) + n


class NullTransport(_Counting):
    def ordered(self, em: Emitter, ev: dict[str, Any], payload: bytes) -> None:
        pass

    def latest(self, em: Emitter, kind: str, key: str, fields: dict[str, Any], t: float) -> None:
        pass


class FileTransport(_Counting):
    """Newline-delimited JSON, appended as it happens. Progress and metrics at most ten times a
    second per key, and their last values at close."""

    def __init__(self, path: str) -> None:
        super().__init__()
        self.f = open(path, "a", encoding="utf-8")  # noqa: SIM115 - held for the run
        self.last: dict[tuple[str, str], float] = {}
        self.pending: dict[tuple[str, str], tuple[dict[str, Any], float]] = {}

    def ordered(self, em: Emitter, ev: dict[str, Any], payload: bytes) -> None:
        self.f.write(payload.decode() + "\n")
        self.f.flush()

    def _write_latest(self, em: Emitter, kind: str, fields: dict[str, Any], t: float) -> None:
        ev = {"kind": kind, "t": t, **fields, "seq": em.next_seq(), "v": PROTOCOL}
        if em.run_id:
            ev["run"] = em.run_id
        self.f.write(encode(ev)[0].decode() + "\n")

    def latest(self, em: Emitter, kind: str, key: str, fields: dict[str, Any], t: float) -> None:
        k = (kind, key)
        now = time.monotonic()
        if now - self.last.get(k, -math.inf) >= 0.1:
            self.last[k] = now
            self.pending.pop(k, None)
            self._write_latest(em, kind, fields, t)
            self.f.flush()
        else:
            self.pending[k] = (fields, t)

    def before_ordered(self, em: Emitter) -> None:
        for (kind, _), (fields, t) in self.pending.items():
            self._write_latest(em, kind, fields, t)
        self.pending.clear()

    def close(self, em: Emitter) -> None:
        self.before_ordered(em)
        if self.counts:
            ev = {"kind": "telemetry", "t": time.time(), "seq": em.next_seq(), "v": PROTOCOL, "counts": dict(self.counts)}
            self.f.write(encode(ev)[0].decode() + "\n")
        self.f.close()


class TerminalTransport(_Counting):
    """The view with no supervisor: stages, logs and artifacts as lines as they happen; progress in
    place on a terminal (ten times a second at most), or a line every five seconds otherwise."""

    def __init__(self, stream: TextIO | None = None) -> None:
        super().__init__()
        self.out = stream or sys.stderr
        self.tty = self.out.isatty()
        self.status = ""
        self.last_draw = -math.inf
        self.last_line = -math.inf

    def _line(self, text: str) -> None:
        try:
            if self.tty and self.status:
                self.out.write("\r\x1b[K")
            self.out.write(text + "\n")
            if self.tty and self.status:
                self.out.write(self.status)
            self.out.flush()
        except (OSError, ValueError):
            pass

    def ordered(self, em: Emitter, ev: dict[str, Any], payload: bytes) -> None:
        k = ev["kind"]
        if k == "stage":
            pad = "  " * ev.get("depth", 0)
            if ev.get("phase") == "begin":
                self._line(f"{pad}▸ {ev.get('name')}")
            else:
                tail = "" if ev.get("ok", True) else f" failed: {ev.get('error', '')}"
                self._line(f"{pad}✓ {ev.get('name')} ({ev.get('elapsed', 0.0):.1f}s){tail}")
        elif k == "log":
            self._line(f"[{ev.get('level')}] {ev.get('msg')}")
        elif k == "artifact.declare":
            self._line(f"artifact {ev.get('name')}: {ev.get('path')} ({ev.get('bytes')} bytes)")
        elif k == "error":
            self._line(f"error {ev.get('type')}: {ev.get('msg')}")

    def latest(self, em: Emitter, kind: str, key: str, fields: dict[str, Any], t: float) -> None:
        if kind != "progress":
            return
        self.status = describe_progress({"key": key, **fields})
        now = time.monotonic()
        try:
            if self.tty and now - self.last_draw >= 0.1:
                self.last_draw = now
                self.out.write("\r\x1b[K" + self.status)
                self.out.flush()
            elif not self.tty and now - self.last_line >= 5.0:
                self.last_line = now
                self.out.write(self.status + "\n")
                self.out.flush()
        except (OSError, ValueError):
            pass

    def close(self, em: Emitter) -> None:
        try:
            if self.status:
                self.out.write(("\r\x1b[K" if self.tty else "") + self.status + "\n")
            if self.counts:
                self.out.write(f"gongoozler: {self.counts}\n")
            self.out.flush()
        except (OSError, ValueError):
            pass


def describe_progress(e: dict[str, Any]) -> str:
    """One line for a progress event: `simulation  42.1%  2024-03-01T10:00:00Z  1.2M/s  eta 12s`."""
    parts = [str(e["key"])]
    done, total = e.get("done"), e.get("total")
    if done is not None and total:
        parts.append(f"{100 * done / total:5.1f}%")
    elif done is not None:
        parts.append(f"{done:,}" if isinstance(done, int) else f"{done:.4g}")
    if e.get("sim") is not None:
        parts.append(time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(e["sim"] / 1e9)))
    if e.get("rate") is not None:
        r = e["rate"]
        parts.append(f"{r / 1e6:.1f}M/s" if r >= 1e6 else f"{r / 1e3:.1f}k/s" if r >= 1e3 else f"{r:.0f}/s")
    if e.get("eta") is not None:
        parts.append(f"eta {e['eta']:.0f}s")
    return "  ".join(parts)


# ---------------------------------------------------------------------------------- emitter


class _Bucket:
    __slots__ = ("burst", "last", "rate", "tokens")

    def __init__(self, rate: float, burst: float) -> None:
        self.rate, self.burst, self.tokens, self.last = rate, burst, burst, time.monotonic()

    def take(self) -> bool:
        now = time.monotonic()
        self.tokens = min(self.burst, self.tokens + (now - self.last) * self.rate)
        self.last = now
        if self.tokens < 1.0:
            return False
        self.tokens -= 1.0
        return True


class Emitter:
    def __init__(self, transport: Transport, *, run_id: str | None = None) -> None:
        self.transport = transport
        self.run_id = run_id
        self.pid = os.getpid()
        self._lock = threading.Lock()
        self._seq = 0
        self._buckets: dict[str, _Bucket] = {}
        self._units: dict[str, str] = {}
        self._closed = False

    def next_seq(self) -> int:
        self._seq += 1
        return self._seq

    def _allowed(self, kind: str) -> bool:
        b = self._buckets.get(kind)
        if b is None:
            b = self._buckets[kind] = _Bucket(LOG_RATE, LOG_BURST) if kind == "log" else _Bucket(EVENT_RATE, EVENT_BURST)
        return b.take()

    def ordered(self, kind: str, fields: dict[str, Any]) -> None:
        cuts = _Cuts()
        clean_fields = clean(fields, cuts)
        t = time.time()
        with self._lock:
            if self._closed:
                return
            if kind not in EXEMPT and not self._allowed(kind):
                self.transport.count("limited")
                return
            self.transport.before_ordered(self)
            ev = {"kind": kind, "t": t, **clean_fields, "seq": self.next_seq(), "v": PROTOCOL}
            if self.run_id:
                ev["run"] = self.run_id
            payload, replaced = encode(ev)
            if cuts.n or replaced:
                self.transport.count("truncated", cuts.n + replaced)
            self.transport.ordered(self, ev, payload)

    def latest(self, kind: str, key: str, fields: dict[str, Any], unit: str | None = None) -> None:
        t = time.time()
        with self._lock:
            if self._closed:
                return
            if unit is not None and self._units.get(key) != unit:
                self._units[key] = unit
                self.transport.before_ordered(self)
                ev = {"kind": "progress.unit", "t": t, "key": key, "unit": unit, "seq": self.next_seq(), "v": PROTOCOL}
                self.transport.ordered(self, ev, encode(ev)[0])
            self.transport.latest(self, kind, key, fields, t)

    def close(self) -> None:
        with self._lock:
            if self._closed:
                return
            self._closed = True
            self.transport.before_ordered(self)
            self.transport.close(self)


# ------------------------------------------------------------------------------ the global one

_emitter: Emitter | None = None
_init = threading.Lock()
_depth: contextvars.ContextVar[int] = contextvars.ContextVar("gongoozler_stage_depth", default=0)


def _make_transport() -> Transport:
    ring = os.environ.get(ENV_RING)
    if ring:
        try:
            return RingTransport(_ring.Ring(ring))
        except OSError as err:
            print(f"gongoozler: cannot open a ring in {ring} ({err}); printing progress here", file=sys.stderr)
            return TerminalTransport()
    where = os.environ.get(ENV_EVENTS) or "stderr"
    if where == "stderr":
        return TerminalTransport()
    if where == "off":
        return NullTransport()
    if where.startswith("file:"):
        return FileTransport(where[5:])
    print(f"gongoozler: {ENV_EVENTS}={where!r} is not stderr, file:PATH or off; printing progress here", file=sys.stderr)
    return TerminalTransport()


def _get() -> Emitter:
    global _emitter
    e = _emitter
    if e is not None and e.pid == os.getpid():
        return e
    with _init:
        if _emitter is None or _emitter.pid != os.getpid():
            _emitter = Emitter(_make_transport(), run_id=os.environ.get(ENV_RUN))
            from . import __version__

            hello: dict[str, Any] = {
                "lib": __version__,
                "pid": os.getpid(),
                "ppid": os.getppid(),
                "python": sys.version.split()[0],
                "script": sys.argv[0] if sys.argv else None,
                "argc": max(len(sys.argv) - 1, 0),
            }
            if os.environ.get(ENV_ARGV) == "1":
                hello["argv"] = sys.argv
            _emitter.ordered("hello", hello)
            _install_hooks()
        return _emitter


def close() -> None:
    """Closes the process's emitter. Runs at exit; tests call it to start afresh."""
    global _emitter
    with _init:
        e, _emitter = _emitter, None
    if e is not None and e.pid == os.getpid():
        e.close()


_hooks_installed = False


def _install_hooks() -> None:
    global _hooks_installed
    if _hooks_installed:
        return
    _hooks_installed = True
    atexit.register(close)
    previous = sys.excepthook

    def excepthook(tp, value, tb):  # type: ignore[no-untyped-def]
        where, frame = None, tb
        while frame is not None:
            where = f"{frame.tb_frame.f_code.co_filename}:{frame.tb_lineno}"
            frame = frame.tb_next
        if _emitter is not None:
            _emitter.ordered("error", {"type": tp.__name__, "msg": str(value), "where": where})
        previous(tp, value, tb)

    sys.excepthook = excepthook
    previous_thread = threading.excepthook

    def thread_hook(args):  # type: ignore[no-untyped-def]
        where, frame = None, args.exc_traceback
        while frame is not None:
            where = f"{frame.tb_frame.f_code.co_filename}:{frame.tb_lineno}"
            frame = frame.tb_next
        if _emitter is not None and args.exc_type is not None:
            name = args.thread.name if args.thread is not None else None
            _emitter.ordered("error", {"type": args.exc_type.__name__, "msg": str(args.exc_value), "where": where, "thread": name})
        previous_thread(args)

    threading.excepthook = thread_hook


def _after_fork() -> None:
    # The child must not write into its parent's ring: the next call makes its own.
    global _emitter
    _emitter = None


os.register_at_fork(after_in_child=_after_fork)


def _line_buffered() -> None:
    """Under a supervisor, prints are written a line at a time. The supervisor sets
    PYTHONUNBUFFERED, so a file that never imports gongoozler still shows its output at once; but
    unbuffered, each print is two writes (the text, then the newline), which more than doubled the
    cost of printing (review/repro/e11_chatty.py). Line-buffered, it is one write a line, still
    shown as it is printed."""
    for stream in (sys.stdout, sys.stderr):
        reconfigure = getattr(stream, "reconfigure", None)
        if reconfigure is not None:
            with contextlib.suppress(Exception):  # a stream replaced by the host is left as it is
                reconfigure(line_buffering=True, write_through=False)


if os.environ.get(ENV_RING):
    _line_buffered()


# --------------------------------------------------------------------------------- public calls


class stage(contextlib.ContextDecorator):
    """A named stage of the run, as a context manager or a decorator. Stages nest."""

    def __init__(self, name: str) -> None:
        self.name = name

    def __enter__(self) -> stage:
        self._depth = _depth.get()
        self._token = _depth.set(self._depth + 1)
        self._t0 = time.monotonic()
        _get().ordered("stage", {"name": self.name, "phase": "begin", "depth": self._depth})
        return self

    def __exit__(self, tp: type[BaseException] | None, value: BaseException | None, tb: TracebackType | None) -> Literal[False]:
        _depth.reset(self._token)
        fields: dict[str, Any] = {"name": self.name, "phase": "end", "depth": self._depth, "elapsed": time.monotonic() - self._t0, "ok": tp is None}
        if tp is not None:
            fields["error"] = f"{tp.__name__}: {value}"
        _get().ordered("stage", fields)
        return False


def _num(x: Any) -> Any:
    if isinstance(x, bool):
        return int(x)
    if isinstance(x, int):
        return x
    try:
        f = float(x)
    except (TypeError, ValueError):
        return None
    return f if math.isfinite(f) else None


def progress(
    key: str, done: float, total: float | None = None, *, unit: str | None = None, sim: int | None = None, rate: float | None = None, eta: float | None = None
) -> None:
    """How far `key` has got: `done` of `total` (in `unit`). `sim` is the simulated time reached,
    in nanoseconds since the epoch. Only the latest value per key is kept."""
    fields: dict[str, Any] = {"key": str(key), "done": _num(done)}
    if total is not None:
        fields["total"] = _num(total)
    if sim is not None:
        fields["sim"] = int(sim)
    if rate is not None:
        fields["rate"] = _num(rate)
    if eta is not None:
        fields["eta"] = _num(eta)
    _get().latest("progress", str(key), fields, unit)


def metric(name: str, value: float, *, at: int | None = None) -> None:
    """A named number as the run goes (NAV, P&L, exposure). `at`: the simulated time, in
    nanoseconds. Only the latest value per name is kept: a series to keep belongs in an artifact."""
    fields: dict[str, Any] = {"name": str(name), "value": _num(value)}
    if at is not None:
        fields["at"] = int(at)
    _get().latest("metric", str(name), fields)


def log(msg: Any, level: str = "info") -> None:
    """A log line. Rate-limited: what is over the limit is counted, not sent."""
    _get().ordered("log", {"level": str(level), "msg": str(msg)})


def emit(kind: str, **fields: Any) -> None:
    """An ordered event of any kind, never coalesced, rate-limited per kind. Events are for the
    control plane: data at the rate of ticks or fills belongs in an artifact."""
    if kind in ("progress", "metric", "progress.unit", "hello", "artifact.declare"):
        raise ValueError(f"{kind} is reserved; use gongoozler.{kind.split('.')[0]}() instead")
    _get().ordered(str(kind), fields)


def artifact(path: str | os.PathLike[str], name: str | None = None) -> Path:
    """Declares a file the run produced. Under a supervisor, the supervisor copies and hashes it out
    of this process, so declaring costs nothing here. Returns the path."""
    p = Path(path).resolve()
    size = p.stat().st_size
    _get().ordered("artifact.declare", {"name": name or p.name, "path": str(p), "bytes": size})
    return p


_artifacts: Path | None = None


def artifacts_dir() -> Path:
    """Where this run writes its artifacts: GONGOOZLER_ARTIFACTS under a supervisor, otherwise
    `gongoozler-out/<time>-<pid>/` in the working directory."""
    global _artifacts
    target = os.environ.get(ENV_ARTIFACTS)
    if target:
        p = Path(target)
    else:
        if _artifacts is None:
            _artifacts = Path("gongoozler-out") / f"{time.strftime('%Y%m%dT%H%M%SZ', time.gmtime())}-{os.getpid()}"
        p = _artifacts
    p.mkdir(parents=True, exist_ok=True)
    return p


def throttled(every_s: float) -> Callable[[], bool]:
    """A wall-clock gate: returns True at most once every `every_s` seconds."""
    last = [-math.inf]

    def ready() -> bool:
        now = time.monotonic()
        if now - last[0] >= every_s:
            last[0] = now
            return True
        return False

    return ready


@contextlib.contextmanager
def _capture(transport: Transport) -> Iterator[Emitter]:
    """Tests: routes this process's events to `transport` for the block."""
    global _emitter
    close()
    with _init:
        _emitter = Emitter(transport)
    try:
        yield _emitter
    finally:
        close()
