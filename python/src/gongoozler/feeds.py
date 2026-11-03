"""The feed contract: how any data library streams data into a backtester (spec/feeds.md).

A data library defines its reads however it likes. What it hands the backtester is a `Feed`: a
named stream of batches,
- each sorted by `ts_init`;
- never going back in time from one batch to the next;
- holding nothing known before it happened (`ts_event <= ts_init`), unless the feed says it carries
  scheduled future events.

A batch may carry its items' timestamps (`Batch.ts`, a list or a numpy array), which a library
reading columns usually has already; otherwise they are read from the items. A feed may declare
what it carries (`kind`, `instruments`) and which version of its data it is (`revision`). As it
is read it is fingerprinted (count, first and last time, and a digest of every timestamp), for
the run's manifest.

`windows` merges several feeds into one stream of windows for Nautilus's streaming loop:
- a window holds every feed's items below a bound that all open feeds have passed, so items
  sharing a timestamp are never split between windows while two feeds are open;
- adding a window's parts in feed order reproduces Nautilus's tie rule, where the stream added
  first wins ("Equal-timestamp replay ordering", docs/concepts/backtesting/apis-and-runs.md).

So streaming a run in windows gives the same result as adding each feed whole, in feed order.
"""

from __future__ import annotations

import bisect
import datetime as dt
import hashlib
import itertools
import operator
from array import array
from collections import deque
from collections.abc import Callable, Iterable, Iterator, Sequence
from dataclasses import dataclass, field
from typing import Any, Literal, cast

_np: Any
try:  # numpy is optional: timestamps handed over as arrays are checked vectorised
    import numpy as _np
except ImportError:  # pragma: no cover - exercised where numpy is absent
    _np = None

Check = Literal["full", "edges"]
"""`full` checks every item (the default); `edges` trusts the order inside a batch and checks only
where batches meet."""

_EPOCH = dt.datetime(1970, 1, 1, tzinfo=dt.UTC)


class FeedError(ValueError):
    """A feed broke the contract. The message names the feed and the batch."""


@dataclass(frozen=True, slots=True)
class Batch:
    """Items in `ts_init` order, and optionally their timestamps (nanoseconds since the epoch):
    `ts` for ts_init, `ts_event` for ts_event, each a list or an array."""

    items: Sequence[Any]
    ts: Sequence[int] | Any | None = None
    ts_event: Sequence[int] | Any | None = None


def to_ns(x: int | str | dt.datetime | dt.date | None) -> int | None:
    """Nanoseconds since the epoch, from an int (taken as nanoseconds), an ISO string, a datetime
    (naive means UTC) or a date (midnight UTC)."""
    if x is None:
        return None
    if isinstance(x, bool):
        raise TypeError("a time is not a bool")
    if isinstance(x, int):
        return x
    if isinstance(x, str):
        x = dt.datetime.fromisoformat(x.replace("Z", "+00:00"))
    if isinstance(x, dt.datetime):
        if x.tzinfo is None:
            x = x.replace(tzinfo=dt.UTC)
        return (x - _EPOCH) // dt.timedelta(microseconds=1) * 1_000
    if isinstance(x, dt.date):
        return to_ns(dt.datetime(x.year, x.month, x.day, tzinfo=dt.UTC))
    raise TypeError(f"not a time: {x!r}")


@dataclass
class Fingerprint:
    """What a feed delivered: how many items, the first and last ts_init, and a BLAKE2b digest of
    every ts_init in order (or, when they were not read, of the batches' edges)."""

    count: int = 0
    first: int | None = None
    last: int | None = None
    basis: str = "ts_init"
    _h: Any = field(default_factory=lambda: hashlib.blake2b(digest_size=16), repr=False)

    @property
    def digest(self) -> str:
        return str(self._h.hexdigest())

    def as_dict(self) -> dict[str, Any]:
        return {"count": self.count, "first": self.first, "last": self.last, "digest": self.digest, "basis": self.basis}


class Feed:
    """A named stream of batches.

    `source` is an iterable of batches, or a function returning one. A function is called on every
    pass, so the feed can be read again. A batch is a `Batch`, or a plain sequence of items that
    have `ts_init`.

    - `start` and `end`, when known, bound the feed's timestamps. They let a driver report progress
      against the whole window, and the feed is checked against them.
    - `kind` (a data type's name, such as "QuoteTick") and `instruments` (instrument IDs) say what
      it carries. A driver checks the instruments exist before it runs.
    - `revision` says which version of its data it is, for the manifest.
    - `future_events`: the feed carries events scheduled ahead of their time (`ts_event` after
      `ts_init`), so the look-ahead check is off.
    """

    def __init__(
        self,
        name: str,
        source: Iterable[Batch | Sequence[Any]] | Callable[[], Iterable[Batch | Sequence[Any]]],
        *,
        start: int | str | dt.datetime | dt.date | None = None,
        end: int | str | dt.datetime | dt.date | None = None,
        kind: str | None = None,
        instruments: Sequence[str] | None = None,
        revision: str | None = None,
        future_events: bool = False,
    ) -> None:
        self.name = name
        self._source = source
        self.start = to_ns(start)
        self.end = to_ns(end)
        if self.start is not None and self.end is not None and self.end < self.start:
            raise ValueError(f"feed {name!r} ends before it starts")
        self.kind = kind
        self.instruments = [str(i) for i in instruments] if instruments else None
        self.revision = revision
        self.future_events = future_events
        self.fingerprint = Fingerprint()

    def __iter__(self) -> Iterator[Batch]:
        source = self._source() if callable(self._source) else self._source
        for b in source:
            yield b if isinstance(b, Batch) else Batch(b)

    def describe(self) -> dict[str, Any]:
        """The feed for a run's manifest: what it declared, and what it delivered."""
        return {
            "name": self.name,
            "kind": self.kind,
            "instruments": self.instruments,
            "revision": self.revision,
            "start": self.start,
            "end": self.end,
            "fingerprint": self.fingerprint.as_dict(),
        }

    def __repr__(self) -> str:
        return f"Feed({self.name!r}, start={self.start}, end={self.end})"


def feed(name: str, source: Any, **kw: Any) -> Feed:
    """A feed from an iterable of batches, or a function returning one (see `Feed`)."""
    return Feed(name, source, **kw)


def from_items(name: str, items: Sequence[Any], *, batch_size: int = 100_000, ts: Sequence[int] | None = None, **kw: Any) -> Feed:
    """A feed over items already in memory, in `ts_init` order, cut into batches."""
    if batch_size < 1:
        raise ValueError("batch_size must be at least 1")

    def batches() -> Iterator[Batch]:
        for i in range(0, len(items), batch_size):
            yield Batch(items[i : i + batch_size], None if ts is None else ts[i : i + batch_size])

    return Feed(name, batches, **kw)


# ------------------------------------------------------------------------- timestamp helpers


def _is_array(ts: Any) -> bool:
    return _np is not None and isinstance(ts, _np.ndarray)


def _sorted(ts: Any) -> int | None:
    """The first index out of order, or None."""
    if _is_array(ts):
        bad = _np.flatnonzero(ts[1:] < ts[:-1])
        return int(bad[0]) + 1 if len(bad) else None
    if all(map(operator.le, ts, itertools.islice(ts, 1, None))):
        return None
    return next(i for i in range(1, len(ts)) if ts[i] < ts[i - 1])


def _lookahead(ts_event: Any, ts: Any) -> int | None:
    """The first index known before it happened (ts_event > ts_init), or None."""
    if _is_array(ts_event) or _is_array(ts):
        bad = _np.flatnonzero(_np.asarray(ts_event) > _np.asarray(ts))
        return int(bad[0]) if len(bad) else None
    if all(map(operator.le, ts_event, ts)):
        return None
    return next(i for i, (e, t) in enumerate(zip(ts_event, ts, strict=True)) if e > t)


def _ts_bytes(ts: Any) -> bytes:
    if _is_array(ts):
        return cast(bytes, _np.ascontiguousarray(ts, dtype="<i8").tobytes())
    return array("q", ts).tobytes()


def checked(f: Feed, check: Check = "full") -> Iterator[tuple[Sequence[Any], Any, int, int]]:
    """Each non-empty batch of `f` as `(items, ts, first, last)`, enforcing the contract and
    fingerprinting the feed. `ts` is None only under `edges` when the batch carries none."""
    if check not in ("full", "edges"):
        raise ValueError(f"check is 'full' or 'edges', not {check!r}")
    fp = f.fingerprint = Fingerprint()
    previous: int | None = None
    for k, b in enumerate(f):
        items = b.items
        n = len(items)
        if n == 0:
            continue
        ts = b.ts
        if ts is not None and len(ts) != n:
            raise FeedError(f"feed {f.name!r}, batch {k}: {len(ts)} timestamps for {n} items")
        if ts is None and check == "full":
            ts = [x.ts_init for x in items]
        if ts is not None:
            if check == "full":
                bad = _sorted(ts)
                if bad is not None:
                    raise FeedError(f"feed {f.name!r}, batch {k}: not sorted by ts_init (item {bad} at {ts[bad]} follows {ts[bad - 1]})")
                if b.ts is not None and (items[0].ts_init != ts[0] or items[-1].ts_init != ts[-1]):
                    raise FeedError(f"feed {f.name!r}, batch {k}: its timestamps do not match its items' ts_init")
                if not f.future_events:
                    te = b.ts_event
                    if te is None and hasattr(items[0], "ts_event"):
                        te = [x.ts_event for x in items]
                    if te is not None:
                        bad = _lookahead(te, ts)
                        if bad is not None:
                            raise FeedError(
                                f"feed {f.name!r}, batch {k}: item {bad} is known before it happened (ts_init {ts[bad]}, ts_event {te[bad]}): "
                                "a look-ahead in the data; a feed of scheduled future events says future_events=True"
                            )
            first, last = int(ts[0]), int(ts[-1])
            fp._h.update(_ts_bytes(ts))
        else:
            first, last = items[0].ts_init, items[-1].ts_init
            if last < first:
                raise FeedError(f"feed {f.name!r}, batch {k}: not sorted by ts_init (it ends at {last}, before it starts at {first})")
            fp.basis = "edges"
            fp._h.update(array("q", (first, last, n)).tobytes())
        if previous is not None and first < previous:
            raise FeedError(f"feed {f.name!r}, batch {k}: goes back in time (it starts at {first}, before the previous batch ended at {previous})")
        if f.start is not None and first < f.start:
            raise FeedError(f"feed {f.name!r}, batch {k}: starts at {first}, before the feed's start {f.start}")
        if f.end is not None and last > f.end:
            raise FeedError(f"feed {f.name!r}, batch {k}: ends at {last}, after the feed's end {f.end}")
        previous = last
        fp.count += n
        fp.first = first if fp.first is None else fp.first
        fp.last = last
        yield items, ts, first, last


@dataclass(frozen=True, slots=True)
class Window:
    """Items to add to an engine together: `(feed index, items)` in feed order, with empty parts
    left out, and the window's first and last timestamps."""

    parts: tuple[tuple[int, Sequence[Any]], ...]
    first: int
    last: int
    count: int


class _Chunk:
    __slots__ = ("items", "pos", "ts")

    def __init__(self, items: Sequence[Any], ts: Any) -> None:
        self.items, self.ts, self.pos = items, ts, 0


class _Buffer:
    """A feed's unconsumed items, as the batches they arrived in: extending it appends a chunk and
    copies nothing, so a long run of equal timestamps costs linear time."""

    __slots__ = ("chunks",)

    def __init__(self) -> None:
        self.chunks: deque[_Chunk] = deque()

    def extend(self, items: Sequence[Any], ts: Any) -> None:
        self.chunks.append(_Chunk(items, ts))

    def empty(self) -> bool:
        return not self.chunks

    def last_ts(self) -> int:
        c = self.chunks[-1]
        return int(c.ts[-1])

    def cut(self, bound: int | None) -> tuple[list[Any], int | None, int | None]:
        """Takes every item below `bound` (all of them when None), in order."""
        out: list[Any] = []
        first = last = None
        while self.chunks:
            c = self.chunks[0]
            n = len(c.items)
            if bound is None:
                end = n
            elif _is_array(c.ts):
                end = int(_np.searchsorted(c.ts, bound, side="left"))
                end = max(end, c.pos)
            else:
                end = bisect.bisect_left(c.ts, bound, c.pos)
            if end > c.pos:
                first = int(c.ts[c.pos]) if first is None else first
                last = int(c.ts[end - 1])
                out.extend(c.items[c.pos : end])
                c.pos = end
            if c.pos < n:
                break
            self.chunks.popleft()
        return out, first, last


def windows(feeds: Sequence[Feed], *, check: Check = "full") -> Iterator[Window]:
    """Merges `feeds` into windows (see the module's notes).

    One feed passes its batches through as windows. Several feeds need every item's timestamp to
    cut at the bound, so their batches' timestamps are read when they carry none, even under
    `edges`.
    """
    if not feeds:
        return
    if len(feeds) == 1:
        for items, _ts, first, last in checked(feeds[0], check):
            yield Window(((0, items),), first, last, len(items))
        return
    yield from _merge(list(feeds), check)


def _merge(feeds: list[Feed], check: Check) -> Iterator[Window]:
    n = len(feeds)
    sources = [checked(f, check) for f in feeds]
    buffers = [_Buffer() for _ in range(n)]
    ended = [False] * n

    def pull(i: int) -> bool:
        """Appends feed i's next batch to its buffer; False once the feed has ended."""
        try:
            items, ts, _first, _last = next(sources[i])
        except StopIteration:
            ended[i] = True
            return False
        if ts is None:
            ts = [x.ts_init for x in items]
        buffers[i].extend(items, ts)
        return True

    for i in range(n):
        pull(i)
    while True:
        open_feeds = [i for i in range(n) if not ended[i]]
        if len(open_feeds) == 1 and all(buffers[j].empty() for j in range(n) if j != open_feeds[0]):
            # One feed is left and nothing else is buffered: nothing can tie with it any more, so its
            # batches pass through whole (a timestamp may span its batches, as within one stream).
            i = open_feeds[0]
            items, first, last = buffers[i].cut(None)
            if items and first is not None and last is not None:
                yield Window(((i, items),), first, last, len(items))
            for batch, _ts, b_first, b_last in sources[i]:
                yield Window(((i, batch),), b_first, b_last, len(batch))
            return
        # Every open feed has items buffered, and will never again yield one below its buffer's last
        # timestamp. So nothing below the smallest of those can still arrive from any feed.
        bound = min(buffers[i].last_ts() for i in open_feeds) if open_feeds else None
        parts: list[tuple[int, Sequence[Any]]] = []
        w_first: int | None = None
        w_last: int | None = None
        count = 0
        for i in range(n):
            if buffers[i].empty():
                continue
            items, lo, hi = buffers[i].cut(bound)
            if items and lo is not None and hi is not None:
                parts.append((i, items))
                w_first = lo if w_first is None else min(w_first, lo)
                w_last = hi if w_last is None else max(w_last, hi)
                count += len(items)
        if parts and w_first is not None and w_last is not None:
            yield Window(tuple(parts), w_first, w_last, count)
            for i in open_feeds:
                if buffers[i].empty():
                    pull(i)
            continue
        if not open_feeds:
            return
        # Nothing lies below the bound. The feeds whose buffers end exactly at it may still hold
        # items at that same timestamp, so read on from them.
        for i in open_feeds:
            if buffers[i].last_ts() == bound:
                pull(i)
