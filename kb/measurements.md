# Measurements

All taken on 3 Nov 2026 on Ashley's Omarchy machine:
- 16 cores, 27 GB of memory, of which about 1.5 GB was free;
- a VM running alongside.

Differences under about 5% are noise. Medians of three runs. The scripts are in `tools/spikes`,
each a locked PEP 723 file.

## Reading progress inside a Nautilus backtest (`progress_cost.py`)

The setup:
- Nautilus 2.0.0rc8 and its built-in EmaCross (Rust);
- 1,000,000 synthetic AUD/USD quotes, one a second (11.6 simulated days);
- loading the data counted in every case.

| How progress is read | Seconds | Against none |
| --- | ---: | --- |
| Not at all | 1.087 | — |
| An actor, no timer | 1.047 | Within noise |
| An actor, 1,000 timer events | 1.083 | Within noise |
| An actor, 10,000 timer events | 1.076 | Within noise |
| An actor, one event per simulated second (1,000,000) | 1.874 | +72% |
| Streamed in 100 batches, read between them | 0.980 | −10% |
| Streamed in 1,000 batches | 0.954 | −12% |

**The rule:** the cost follows the number of Python callbacks, not the size of the data. So:
- read progress between batches (free);
- or, from inside the loop, about 1,000 times per run.

At 200,000 quotes (a 0.2 s run), 10,000 timer events cost +20%: fixed costs dominate a run that
short.

## Streaming changes nothing (`stream_equivalence.py`)

On 300,000 quotes, a single run and runs streamed in 10 and in 1,000 batches gave the same final
balance (1,189,772.00 USD), 984 orders, and the same positions.

## The driver's own cost (`driver_cost.py`)

The same backtest, 1,000,000 quotes in 100 windows, against a hand-written streaming loop, on the
current code:

| How | Seconds | Against the loop |
| --- | ---: | --- |
| A hand-written loop, no progress | 0.947 | — |
| `gongoozler.nautilus.run`, reading timestamps from the items | 0.982 | +3.8% |
| The same, with the feed handing its timestamps over | 0.964 | +1.9% |
| The same, checking only where batches meet | 0.945 | Within noise |

An earlier run, on a stale build whose driver path was the same, gave +7.4% when reading
timestamps from items. Reading `ts_init` from 1,000,000 quotes in Python takes 51 ms, and checking
their order 19 ms. With a Python strategy, all of this shrinks against the strategy's own cost.

## uv (`stream_equivalence.py`)

| Step | Time |
| --- | --- |
| `uv lock --script` on a Nautilus file | 0.14–0.26 s |
| First `uv run`, empty cache: downloading the 74 MB wheel, installing, running | 8.8 s |
| `uv run` with its environment cached | 1.45 s, against 1.39 s for the bare run |

So a box whose image carries a warm uv cache builds a typical environment in well under a second.

## The example (`examples/ema_cross.py`)

Run through `gongoozler run`: 864,000 quotes (10 days), generated batch by batch, streamed in 18
windows in 1.7 s at about 520,000 quotes a second. The memory peak was 222.5 MiB, because only
one batch is ever held.

## The emitter

Into a shared-memory ring, as under a supervisor (`make bench`, `tools/bench/bench.py`):

| Call | Cost |
| --- | --- |
| `gz.progress(key, done, total)`: a slot write | 1.9 µs |
| An ordered event (`gz.emit`): a ring record | 4.3 µs |

Before the ring (a buffer and a sender thread), progress cost 0.96 µs in the call but the
sender's work competed for Python's lock: a strategy loop ran 67% slower than its own work plus
the calls explained (`review/repro/e7_gil.py`). Now nothing else in the run's process wants the
lock. `test_emitting_progress_costs_microseconds` fails a build where a call costs more than
10 µs.

## The budgets (`make bench`)

On 3 Nov 2026, after the review's changes. Each side's best of five alternated runs (seven since):

| Budget | Measured | Bound |
| --- | --- | --- |
| Telemetry: EmaCross over 300,000 quotes, progress and snapshots every 250 ms, under the supervisor, against events off | +0.41% (0.336 s against 0.334 s) | ≤ 1% |
| A progress update | 1.90 µs | ≤ 5 µs |
| An ordered event | 4.28 µs | ≤ 25 µs |
| Ingest: `BacktestNode` streaming a catalog of 1,000,000 quotes, no strategy, against Nautilus's engine given the same quotes from memory | 0.95× (1.25M against 1.32M quotes/s) | ≥ 0.85× |
| The same, with EmaCross | 1.00M quotes/s | for the record |

The review set ingest at a million quotes a second on a core. That holds here (1.2M), but not on
GitHub's shared runners, where the same code ingested 0.95M (and the in-memory engine about as
much): a throughput of its own measures the machine as much as the code. So the budget CI holds is
relative, the catalog path against Nautilus's own engine on the same machine and data, and the
quotes a second are reported beside it. The same CI run measured telemetry at +0.81%, a progress
update at 2.42 µs and an ordered event at 6.33 µs, all inside their budgets.

The first event of a run makes its ring (an 8 MiB file in `/dev/shm`); that is a cost of starting,
which the supervisor reports as `startup`, and the bench pays it before timing. Counted inside a
0.12 s run it was +7.5%.

## The review's reproductions, after

The scripts are in `review/repro`; each finding is a test now (the review's resolution section).

| Finding | Before | After |
| --- | --- | --- |
| C1, a NaN (`e1_nan.py`) | no events at all; status `ok` | every event stored; the NaN as `null` |
| C2, a 5 MiB line (`e2_big.py`) | the stream ended | its string cut to 64 Ki characters and counted (`run.end`: "1 events cut to fit"); the stream goes on |
| C3, the CLI killed (`e3_orphan.py`) | the run lived on with parent 1; `ls` said `running` | dead within a second (0.07 s here), `lost` |
| H1, a leftover `sleep` (`e4_leak.py`) | still running after the CLI | stopped, and named in `run.end` |
| H2, signals through uv (`e5_signals.py`) | each delivered twice | once |
| H3, four forked workers (`e6_cow.py`) | 788.3 MiB reported for 197 MiB | 200.1 MiB (PSS) |
| M1, a same-timestamp burst (`e9_merge.py`) | 240 → 1,250 ns an item as it doubled | 20 ns an item, flat |
| M2, 100,000 printed lines (`e11_chatty.py`, under a pseudo-terminal) | 0.96 s live, 0.24 s bare; 12.6 MB written | importing gongoozler: 0.241 s live, 0.256 s bare; not importing it: 0.36 s (Python's unbuffered writes, which keep output immediate); 1.2 MB written |
| H6, a catalog (`e12_catalog.py`) | 1.05 s a run from objects that took 1.01 s to build; 0.97 s from the catalog | `run_node` keeps the same results as `run`, every file |

The CLI's own fixed cost on a one-line file: 15 ms, or 32 ms with a cgroup scope.
