# Results: what a run keeps

This page is normative. `python/src/gongoozler/results.py`, `nautilus.py` and `fi.py` write results;
`gongoozler diff` and `gongoozler rerun` compare them. Their tests are
`python/tests/test_results.py`, `test_catalog.py`, and the CLI's
`TestARerunReproducesItsResultsAndDiffShowsWhatChanged`.

A backtest platform is for comparing runs. So a run keeps its results as data, written so that
**the same run on the same inputs writes the same bytes**, and a rerun proves itself by its
hashes.

## Artifacts

A run writes files into `gz.artifacts_dir()` (`GONGOOZLER_ARTIFACTS` under a supervisor, else
`gongoozler-out/TIME-PID/`) and declares each with `gz.artifact(path)`. Declaring costs nothing in
the run: it sends an `artifact.declare` event, and the supervisor copies and hashes the file out of
the run's process (`artifact` events, with `sha256`). In a box, artifacts go to the server as
content-addressed blobs (spec/server.md).

## What the Nautilus drivers keep

`gongoozler.nautilus.run` and `run_node` keep, unless asked not to (`keep=False`):

| File | What |
| --- | --- |
| `orders.jsonl` | Every order, as Nautilus's `to_dict()` |
| `fills.jsonl` | Every fill |
| `positions.jsonl` | Every position |
| `account.jsonl` | Every account state, per venue |
| `returns.jsonl` | The returns series, `{ts, return}` |
| `stats.json` | Nautilus's statistics: `pnls`, `returns`, `general`, and the order and position counts |
| `series.jsonl` | Balances, P&L, open positions and orders on an exact simulated-time grid (below) |
| `manifest.json` | What produced the results (below) |

**Deterministic.** Every file is canonical JSON: keys sorted, no spaces, non-finite numbers as
`null`, one object a line for tables (`gongoozler.results.canonical`). Fields that differ
between identical runs are left out: Nautilus's random event IDs (`init_id`, `event_id`,
`causation_id`) and nested event lists (`gongoozler.nautilus.UNSTABLE`).

**Evidence:**
- the same backtest in two fresh processes writes byte-identical files, fills and `trade_id`s
  included (`test_the_same_run_in_another_process_writes_the_same_bytes`);
- the same data in batches of 1,000 and 7,919 keeps the same results
  (`test_results_do_not_depend_on_how_the_data_was_batched`);
- a run over a Parquet catalog in Rust (`run_node`) keeps exactly the results of the same feed
  streamed as Python objects, every file (`test_a_catalog_run_trades_exactly_as_the_streamed_run`);
- a run here and the same run in a box keep the same results
  (`TestAFileRunOnTheServerKeepsTheSameResultsAsHere`).

**A failed run keeps what it had.** If the feed breaks the contract or the strategy raises, the
engine is still ended, the results so far are kept, the manifest says `"complete": false`, and the
error is raised (`test_a_run_that_fails_keeps_what_it_had_and_says_it_is_incomplete`).

## What the engine's driver keeps

`gongoozler.fi.run` and `sweep` run the fixed income engine (`fi_engine`, `gongoozler[fi]`) and keep,
unless asked not to (`keep_results=False`), each prefixed by the book's name in a sweep:

| File | What |
| --- | --- |
| `ledger.jsonl` | The accounts at each time: NAV, cash, income, price, principal, other, financing, costs, currency, capital, unsettled, overdue, NAV per unit, turnover, holdings |
| `orders.jsonl` | Every order's life (with the book's `journal`) |
| `journal.jsonl` | Every transfer with its cause (with the book's `journal`) |
| `holdings.json` | The final holdings and cash |
| `stats.json` | The run (final NAV, trades, guards, failure, the identity's largest error, the output's version), its performance, and its order statistics |
| `manifest.json` | What produced it: the parameters and packages, and the engine's own manifest (series versions and provenance, the knowledge policy, models, the calendar, the book's settings and step order, the mode, approximations, the fingerprint) |

Times are integer nanoseconds and dates ISO strings, so the same run writes the same bytes in any
process (`test_an_engine_run_keeps_its_results_and_the_same_run_writes_the_same_bytes`), and a
rerun reproduces them (`TestAnEngineRunShowsItsProgressAndItsResultsCompare`).

## The series: simulated time, not wall time

When the feeds declare a start and an end, the series is sampled by an actor on a simulated-time
timer, at `start + k × (end − start) / series_points` (default 1,000 points): the same grid
however the data was batched and however fast the machine. Live metrics during a run (portfolio
snapshots every 250 ms of wall time) are for watching; the series is for comparing
(`test_the_series_lies_on_an_exact_simulated_time_grid`).

## The manifest

```json
{
  "gongoozler": "0.1.0",
  "params": {"fast": 12, "slow": 50},
  "packages": {"gongoozler": "0.1.0", "nautilus_trader": "2.0.0rc8"},
  "python": "3.13.15",
  "inputs": {
    "feeds": [{"name": "quotes", "kind": "QuoteTick", "instruments": ["AUD/USD.SIM"], "revision": "2026-10-31",
               "start": 1546383600000000000, "end": 1546403599000000000,
               "fingerprint": {"count": 20000, "first": 1546383600000000000, "last": 1546403599000000000,
                               "digest": "e6e4…", "basis": "ts_init"}}],
    "run": {"windows": 7, "items": 20000, "first": 1546383600000000000, "last": 1546403599000000000, "complete": true}
  },
  "environment": {"platform": "Linux-…", "machine": "x86_64", "node": "…"}
}
```

- `params` are the values the file settled on (`gz.params`).
- Each feed's `fingerprint` is what it delivered: the count, the first and last `ts_init`, and a
  BLAKE2b-128 digest of every `ts_init` in order (or of each batch's edges, with
  `basis: "edges"`, when only edges were read). A catalog feed carries the fingerprint of the feed
  it was written from.
- `environment` is where it ran, kept apart from `inputs`: comparing two runs' inputs ignores it.

A run file can write its own results the same way: `gongoozler.results.write_json(name, obj)`,
`write_jsonl(name, rows)` and `manifest(**inputs)`.

## Comparing runs

`gongoozler diff A B` compares two runs:
- **inputs:** the file's and lock's hashes, the parameters given, the arguments, gongoozler's
  version;
- **results:** each artifact by SHA-256; for one that differs, the first line that does, and for
  JSON objects the fields that do (`stats.json: line 1: general.Sharpe Ratio (252 days)`).

It exits 0 when every artifact is the same, and 1 otherwise.

`gongoozler rerun A` runs A again: the same file (the copy kept with the run if the file has
changed since), its lock, parameters and arguments, from the same working directory, here or on
the server as A was. Then it compares, and says `reproduced: N results byte for byte`, or what
differs, exiting 1. A file with no lock may resolve different packages on a rerun: that is what
the lock on first run is for (spec/run-file.md).
