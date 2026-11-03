# Reproductions for the 3 Nov 2026 review

Each script reproduces one finding in [../2026-11-03.md](../2026-11-03.md). They were run on the
uncommitted working tree of 3 Nov 2026. Set up once, from the repository's root:

```sh
go build -o /tmp/gz ./cmd/gongoozler
export GONGOOZLER_STATE=/tmp/gz-state PYTHONPATH=$PWD/python/src
```

| Finding | Script | How to run it | What it showed |
| --- | --- | --- | --- |
| C1 | `e1_nan.py` | `/tmp/gz run --json --launcher python3 review/repro/e1_nan.py` | No events from the run at all; a traceback from the sender thread; status `ok` |
| C2 | `e2_big.py` | as above | `hello`, the first log, then `bufio.Scanner: token too long`; nothing after |
| C3 | `e3_orphan.py` | start `/tmp/gz run --launcher python3 review/repro/e3_orphan.py &`, then `kill -9` the CLI | The run lives on with parent 1; `ls` shows it `running`; 0 events stored |
| H1 | `e4_leak.py` | `/tmp/gz run --launcher python3 review/repro/e4_leak.py`, then check the PID in `/tmp/e4.pid` | `sleep 41` still running after the CLI exits |
| H2 | `e5_signals.py` | `/tmp/gz run e5_signals.py` (real uv), then send SIGTERM or SIGINT to the CLI | `received: ['SIGTERM', 'SIGTERM']`, and the same for SIGINT |
| H3 | `e6_cow.py` | `/tmp/gz run --plain --launcher python3 review/repro/e6_cow.py` | Actual 197 MiB (summed PSS); reported 788.3 MiB |
| H4 | `e7_rate.py`, `e7_gil.py` | `/tmp/gz run --json --launcher python3 review/repro/e7_rate.py 300000`; `e7_gil.py` the same way | 90,659 of 300,000 dropped, all counted; a strategy loop 67% slower than its own work plus the emit calls explain |
| H6 | `e12_catalog.py` | `cd python && GONGOOZLER_EVENTS=off uv run python ../review/repro/e12_catalog.py` | 1.05 s a run on Python objects that took 1.01 s to build; 0.97 s from the catalog; the same 3,281 orders |
| M1 | `e9_merge.py` | `GONGOOZLER_EVENTS=off python3 review/repro/e9_merge.py` | 240 → 1,250 ns per item as a same-timestamp burst doubles: quadratic |
| M2 | `e11_chatty.py` | under `script -qec` (a pseudo-terminal), with and without the CLI | 0.24 s bare, 0.96 s live, 12.6 MB written for 100,000 lines |
| Not an issue | `e8_fragment.py` | `cd python && GONGOOZLER_EVENTS=off uv run python ../review/repro/e8_fragment.py` | 16,676 windows cost about 3.6 µs each; the results are identical; no slower than one run |

`go test -race ./...` shows the data race in `TestAnInterruptReachesTheRunAndIsReported` (M4). It
also fails `TestARunsEventsAreStoredInTheOrderTheyHappened`, which is flaky.

Run the PID-file scripts from a shell whose own command line does not contain the strings they
search for. `pgrep -f`/`pkill -f` with such a pattern matches, and kills, the shell itself.
