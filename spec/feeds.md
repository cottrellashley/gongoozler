# The feed contract

How any data library streams data into a backtester. This page is normative:
`python/src/gongoozler/feeds.py` and `catalog.py` implement it, and `python/tests/test_feeds.py`,
`test_nautilus.py`, `test_results.py` and `test_catalog.py` check it.

## Who does what

- **The data library defines its reads.** What it reads, from where, how it authenticates, joins
  and maps: all of that is its own business, and gongoozler knows nothing of it.
- **It then defines its feed streams:** what it hands the backtester, in the form below.
- **gongoozler checks each stream, merges several, and drives the backtester with them,**
  reporting progress as it goes.

## A feed

A `Feed` has a `name`, and a `source` of batches: an iterable, or a function returning one. A
function is called on each pass, so the feed can be read again. It may declare:
- `start` and `end` (nanoseconds since the epoch, or an ISO time, datetime or date): the window
  it covers, for progress, the series grid and the checks;
- `kind` (a data type's name, such as `"QuoteTick"`) and `instruments` (instrument IDs): what it
  carries. A driver checks the instruments exist in the engine before anything runs;
- `revision`: which version of its data it is (a vintage, a snapshot ID), for the manifest;
- `future_events=True`: it carries events scheduled ahead of their time (below).

A batch is a `Batch(items, ts=None, ts_event=None)`, or a plain sequence of items. Items are what
the engine takes: for Nautilus, its data objects, each with `ts_init`. `ts` and `ts_event`, when
given, hold the items' timestamps, as lists or numpy arrays. A library that reads columns usually
has them already: handing them over saves reading them back from every item, and arrays are
checked vectorised.

**A feed promises:**
1. every batch is sorted by `ts_init`;
2. no batch starts before the previous batch ended. Equal timestamps may continue into the next
   batch;
3. if declared, every item lies within `[start, end]`;
4. if given, `ts` has one timestamp per item, and agrees with the items;
5. **nothing is known before it happened:** `ts_event <= ts_init` for every item that has a
   `ts_event`, unless the feed declares `future_events=True` (an economic calendar, say, whose
   events are scheduled ahead). A breach is a look-ahead in the data, the error a backtest most
   needs caught.

Empty batches are skipped.

**Checks.** Every promise is checked, and a broken one raises `FeedError`, naming the feed, the
batch and the item (`feed 'news', batch 1: item 1 is known before it happened (ts_init 4,
ts_event 9): a look-ahead in the data`). Under `check="full"` (the default) every item is checked.
Under `check="edges"` only first and last timestamps are read: for a library whose data is
trusted.

**Fingerprints.** As a feed is read it is fingerprinted: its count, its first and last
`ts_init`, and a BLAKE2b-128 digest of every `ts_init` in order (of each batch's edges under
`edges`). The same data fingerprints the same however it is batched, as lists or arrays, and a
single moved timestamp changes the digest. `feed.describe()` gives what it declared and what it
delivered, for the run's manifest (spec/results.md).

## Several feeds

`windows(feeds)` merges feeds into windows: what to add to the engine together. Each window holds
every feed's items below a bound that all open feeds have passed (the smallest of their buffered
last timestamps). The guarantees:
- every item appears once, in its feed's order;
- windows never go back in time;
- a timestamp is never split between windows while two or more feeds are open. Once one feed is
  left alone, its batches pass through whole;
- adding a window's parts in feed order reproduces Nautilus's tie rule. "Across streams: The
  stream registered by the earlier `add_data()` call takes priority. Within a stream: Items
  retain their order in the input list" (Nautilus, `docs/concepts/backtesting/apis-and-runs.md`).

So streaming feeds in windows replays exactly what adding each feed whole, in feed order, would
replay. Merging several feeds needs every timestamp to cut at the bound, so they are read from
the items when a batch carries none, even under `check="edges"`.

**Linear time.** A feed's unconsumed items are kept as the batches they arrived in, and a window
is cut from them by binary search, so a long run of equal timestamps across many batches costs
linear time: about 20 ns an item whatever the run's length (review M1, where it had been
quadratic; `test_merging_a_long_run_of_equal_timestamps_takes_linear_time`).

**Evidence:**
- 200 randomised cases (clustered and equal timestamps, empty batches, 2 to 4 feeds) replay
  exactly as adding each feed whole (`test_streaming_several_feeds_…`).
- On Nautilus 2.0.0rc8, two feeds with over 5,000 tied timestamps give the same final account,
  orders, positions, timer firings, and the same sequence of every quote an actor saw
  (`test_two_feeds_keep_nautilus_tie_order_and_timers`).

## Driving Nautilus

`gongoozler.nautilus.run(engine, feeds)` runs Nautilus's own streaming loop: `add_data` for each
part, `run(streaming=True)` and `clear_data` per window, then `end()`. Memory is bounded by a
window. Progress is read between windows, with nothing inside the engine's loop:
- simulated time against the feeds' `[start, end]` when every feed declares an end; items
  otherwise;
- rate and estimated finish;
- each venue's balances and P&L, and the counts of open positions and orders.

Each part is added with Nautilus's default sorting: Nautilus 2.0 refuses data flagged unsorted
until `sort_data()` is called, so skipping the sort saves nothing. What it costs is in
`kb/measurements.md`. The run's results are kept as spec/results.md describes.

## Catalog feeds: pull once, run many

Building data as Python objects costs about as much as simulating it, and every run of a sweep
builds it again. So a feed can be written once to a Nautilus `ParquetDataCatalog`, and every run
after streams it with `BacktestNode`, in Rust, with no Python object per item:
- `gongoozler.catalog.write(path, feed, instruments=[...])` checks and fingerprints the feed as it
  writes it, by data type, about `rows_per_file` items a file, never splitting a timestamp across
  files (the catalog refuses files whose intervals overlap). It returns a `CatalogFeed` per data
  type: its path, type, instruments (or bar types), window, count and the feed's fingerprint;
- `gongoozler.catalog.cached(root, key, build)` writes a key's catalog only the first time any
  run asks for it: each key is a catalog of its own; a write that failed part-way is written
  again; runs asking at once wait for the one writing;
- `gongoozler.nautilus.run_node(venues=..., data=catalog_feeds, strategies=...)` runs a
  `BacktestNode` over them, with progress from an actor's timer (about a thousand updates a run),
  and keeps the same results as `run`.

**Evidence:** over the same feed, `run_node` keeps exactly the results `run` keeps, every file
(`test_a_catalog_run_trades_exactly_as_the_streamed_run`). Ingest from a catalog runs at about
1.2 million quotes a second on one core (`make bench`).
