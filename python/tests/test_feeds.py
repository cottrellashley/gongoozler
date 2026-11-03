"""The feed contract (spec/feeds.md): what a feed must promise, how a broken promise is reported,
and the merge that streams several feeds in windows without changing what an engine sees."""

from __future__ import annotations

import datetime as dt
import itertools
import random
from dataclasses import dataclass

import pytest

import gongoozler as gz
from gongoozler.feeds import FeedError, checked


@dataclass(frozen=True)
class Item:
    ts_init: int
    feed: int = 0
    n: int = 0


def items(ts: list[int], feed: int = 0) -> list[Item]:
    return [Item(t, feed, i) for i, t in enumerate(ts)]


def test_times_convert_to_nanoseconds() -> None:
    assert gz.to_ns(5) == 5
    assert gz.to_ns("1970-01-01T00:00:01Z") == 1_000_000_000
    assert gz.to_ns(dt.datetime(1970, 1, 1, 0, 0, 0, 1)) == 1_000
    assert gz.to_ns(dt.date(1970, 1, 2)) == 86_400 * 10**9
    assert gz.to_ns(None) is None
    with pytest.raises(TypeError):
        gz.to_ns(True)


def test_a_batch_out_of_order_names_the_feed_batch_and_item() -> None:
    f = gz.feed("quotes", [items([1, 2]), items([3, 5, 4])])
    with pytest.raises(FeedError, match=r"feed 'quotes', batch 1: not sorted by ts_init \(item 2 at 4 follows 5\)"):
        list(checked(f))


def test_a_batch_that_goes_back_in_time_is_refused() -> None:
    f = gz.feed("quotes", [items([1, 5]), items([4, 6])])
    with pytest.raises(FeedError, match="batch 1: goes back in time"):
        list(checked(f))


def test_equal_timestamps_may_continue_into_the_next_batch() -> None:
    f = gz.feed("quotes", [items([1, 5]), items([5, 6])])
    assert [b[2:] for b in checked(f)] == [(1, 5), (5, 6)]


def test_a_feed_is_held_to_its_declared_window() -> None:
    with pytest.raises(FeedError, match="before the feed's start"):
        list(checked(gz.feed("q", [items([1, 2])], start=2)))
    with pytest.raises(FeedError, match="after the feed's end"):
        list(checked(gz.feed("q", [items([1, 3])], end=2)))


def test_timestamps_a_library_supplies_must_match_its_items() -> None:
    with pytest.raises(FeedError, match="2 timestamps for 3 items"):
        list(checked(gz.feed("q", [gz.Batch(items([1, 2, 3]), ts=[1, 2])])))
    with pytest.raises(FeedError, match="do not match"):
        list(checked(gz.feed("q", [gz.Batch(items([1, 2, 3]), ts=[1, 2, 4])])))


def test_edges_mode_reads_only_where_batches_meet() -> None:
    reads = []

    class Counted:
        def __init__(self, t: int) -> None:
            self._t = t

        @property
        def ts_init(self) -> int:
            reads.append(self._t)
            return self._t

    f = gz.feed("q", [[Counted(t) for t in range(1000)]])
    assert [b[2:] for b in checked(f, "edges")] == [(0, 999)]
    assert len(reads) == 2


def test_a_feed_made_by_a_function_can_be_read_twice() -> None:
    f = gz.from_items("q", items(list(range(10))), batch_size=3)
    assert [len(b.items) for b in f] == [3, 3, 3, 1]
    assert [len(b.items) for b in f] == [3, 3, 3, 1]


def test_one_feed_passes_its_batches_through_as_windows() -> None:
    f = gz.feed("q", [items([1, 2]), [], items([2, 3])])
    w = list(gz.windows([f]))
    assert [(x.first, x.last, x.count) for x in w] == [(1, 2, 2), (2, 3, 2)]


def random_feed(rng: random.Random, k: int, n: int) -> tuple[gz.Feed, list[Item]]:
    """A feed with clustered, often equal timestamps, cut into random batches (some empty)."""
    t, ts = 0, []
    for _ in range(n):
        t += rng.choice([0, 0, 1, 2, 5])
        ts.append(t)
    data = items(ts, k)
    batches, i = [], 0
    while i < len(data):
        size = rng.choice([0, 1, 2, 7, 50])
        batches.append(data[i : i + size])
        i += size
    with_ts = rng.random() < 0.5
    source = [gz.Batch(b, [x.ts_init for x in b]) if with_ts else b for b in batches]
    return gz.feed(f"f{k}", source), data


def replay(windows: list[gz.Window]) -> list[Item]:
    """What a Nautilus engine replays from windows added in order: within a window, ascending
    ts_init, ties to the part added first, then input order."""
    out: list[Item] = []
    for w in windows:
        tagged = [(x.ts_init, order, j, x) for order, (_, part) in enumerate(w.parts) for j, x in enumerate(part)]
        out.extend(x for *_, x in sorted(tagged, key=lambda r: r[:3]))
    return out


@pytest.mark.parametrize("seed", range(200))
def test_streaming_several_feeds_in_windows_replays_exactly_as_adding_each_whole(seed: int) -> None:
    rng = random.Random(seed)
    made = [random_feed(rng, k, rng.randint(0, 120)) for k in range(rng.randint(2, 4))]
    feeds = [f for f, _ in made]
    w = list(gz.windows(feeds, check=rng.choice(["full", "edges"])))
    for k, (_, data) in enumerate(made):
        assert [x for win in w for i, part in win.parts if i == k for x in part] == data, "each feed's items, once, in order"
    for a, b in itertools.pairwise(w):
        assert a.last <= b.first, "windows never go back in time"
        if a.last == b.first:
            feeds_at = {i for win in (a, b) for i, part in win.parts if any(x.ts_init == a.last for x in part)}
            assert len(feeds_at) == 1, "a timestamp spans two windows only within the one feed left"
    assert all(win.count == sum(len(p) for _, p in win.parts) > 0 for win in w)
    whole = sorted((x for _, data in made for x in data), key=lambda x: (x.ts_init, x.feed, x.n))
    assert replay(w) == whole


def test_a_feed_that_ends_early_no_longer_holds_the_others_back() -> None:
    a = gz.feed("a", [items([1, 2], 0)])
    b = gz.feed("b", [items([1, 3], 1), items([9, 10], 1)])
    w = list(gz.windows([a, b]))
    assert [x.ts_init for win in w for _, p in win.parts for x in p] == [1, 1, 2, 3, 9, 10]
    assert all(win.parts for win in w)


def test_once_one_feed_is_left_its_batches_pass_through_whole() -> None:
    # Each feed's items at its own last timestamp wait until it is known to have ended; after that,
    # the long feed's batches pass through as they come.
    short = gz.feed("ratings", [items([0, 3], 0)])
    long = gz.feed("quotes", [items([1, 2, 5], 1), items([5, 6], 1), items([6, 9], 1), items([9, 11], 1)])
    w = list(gz.windows([short, long]))
    assert [[(i, [x.ts_init for x in p]) for i, p in win.parts] for win in w] == [
        [(0, [0]), (1, [1, 2])],
        [(0, [3])],
        [(1, [5])],
        [(1, [5, 6])],
        [(1, [6, 9])],
        [(1, [9, 11])],
    ]


def test_an_empty_feed_merges_as_nothing() -> None:
    w = list(gz.windows([gz.feed("a", []), gz.feed("b", [items([4, 5], 1)])]))
    assert [(x.first, x.last) for x in w] == [(4, 5)]


@dataclass(frozen=True)
class Event:
    ts_init: int
    ts_event: int


def test_an_item_known_before_it_happened_is_a_look_ahead() -> None:
    f = gz.feed("news", [[Event(1, 1), Event(2, 1)], [Event(3, 3), Event(4, 9)]])
    with pytest.raises(FeedError, match=r"feed 'news', batch 1: item 1 is known before it happened \(ts_init 4, ts_event 9\).*future_events=True"):
        list(checked(f))


def test_a_feed_of_scheduled_events_says_so_and_is_not_held_to_it() -> None:
    f = gz.feed("calendar", [[Event(1, 50), Event(2, 60)]], future_events=True)
    assert [b[2:] for b in checked(f)] == [(1, 2)]


def test_timestamps_a_library_supplies_are_checked_for_look_ahead_too() -> None:
    f = gz.feed("q", [gz.Batch(items([1, 2, 3]), ts=[1, 2, 3], ts_event=[1, 2, 4])])
    with pytest.raises(FeedError, match=r"item 2 is known before it happened \(ts_init 3, ts_event 4\)"):
        list(checked(f))


def test_timestamps_as_arrays_are_checked_vectorised_like_lists() -> None:
    np = pytest.importorskip("numpy")
    xs = items([1, 2, 2, 5])
    as_list = gz.feed("q", [gz.Batch(xs, ts=[1, 2, 2, 5], ts_event=[0, 2, 1, 5])])
    as_array = gz.feed("q", [gz.Batch(xs, ts=np.array([1, 2, 2, 5]), ts_event=np.array([0, 2, 1, 5]))])
    assert [b[2:] for b in checked(as_list)] == [b[2:] for b in checked(as_array)] == [(1, 5)]
    assert as_list.fingerprint.as_dict() == as_array.fingerprint.as_dict()
    with pytest.raises(FeedError, match=r"item 2 at 1 follows 2"):
        list(checked(gz.feed("q", [gz.Batch(items([1, 2, 1]), ts=np.array([1, 2, 1]))])))
    with pytest.raises(FeedError, match=r"item 1 is known before it happened \(ts_init 2, ts_event 3\)"):
        list(checked(gz.feed("q", [gz.Batch(items([1, 2]), ts=np.array([1, 2]), ts_event=np.array([1, 3]))])))
    merged = list(gz.windows([gz.feed("a", [gz.Batch(items([1, 3, 3]), ts=np.array([1, 3, 3]))]), gz.feed("b", [items([2, 3, 4], 1)])]))
    assert [(w.first, w.last, [(i, [x.n for x in part]) for i, part in w.parts]) for w in merged] == [
        (1, 2, [(0, [0]), (1, [0])]),
        (3, 3, [(0, [1, 2]), (1, [1])]),
        (4, 4, [(1, [2])]),
    ]


def test_a_feed_is_fingerprinted_the_same_however_it_is_batched() -> None:
    ts = sorted(random.Random(7).choices(range(10_000), k=5_000))
    prints = []
    for size in (1, 97, 5_000):
        f = gz.from_items("q", items(ts), batch_size=size)
        assert sum(len(b[0]) for b in checked(f)) == 5_000
        prints.append(f.fingerprint.as_dict())
    assert prints[0] == prints[1] == prints[2]
    assert prints[0]["count"] == 5_000 and (prints[0]["first"], prints[0]["last"]) == (ts[0], ts[-1]) and prints[0]["basis"] == "ts_init"
    other = gz.from_items("q", items([*ts[:-1], ts[-1] + 1]))
    list(checked(other))
    assert other.fingerprint.digest != prints[0]["digest"], "one timestamp moved changes the digest"
    edges = gz.from_items("q", items(ts), batch_size=97)
    list(checked(edges, "edges"))
    assert edges.fingerprint.basis == "edges" and edges.fingerprint.count == 5_000


def test_a_feed_describes_what_it_declared_and_what_it_delivered() -> None:
    f = gz.from_items("q", items([1, 2, 3]), start=0, end=10, kind="QuoteTick", instruments=["AUD/USD.SIM"], revision="v7")
    list(checked(f))
    d = f.describe()
    assert {k: d[k] for k in ("name", "kind", "instruments", "revision", "start", "end")} == {
        "name": "q",
        "kind": "QuoteTick",
        "instruments": ["AUD/USD.SIM"],
        "revision": "v7",
        "start": 0,
        "end": 10,
    }
    assert d["fingerprint"]["count"] == 3


def test_merging_a_long_run_of_equal_timestamps_takes_linear_time() -> None:
    import time

    def burst(batches: int, size: int = 2_000):
        for _ in range(batches):
            yield [Item(0)] * size
        yield [Item(1)]

    def cost(batches: int) -> float:
        best = float("inf")
        for _ in range(3):
            a = gz.feed("bars", lambda: burst(batches))
            b = gz.feed("other", [[Item(0, 1), Item(5, 1)]])
            t = time.perf_counter()
            n = sum(w.count for w in gz.windows([a, b], check="edges"))
            best = min(best, time.perf_counter() - t)
            assert n == batches * 2_000 + 3
        return best

    small, large = cost(50), cost(400)
    assert large / small < 16, f"8x the items took {large / small:.1f}x the time (quadratic would be 64x)"
