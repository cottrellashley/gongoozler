# gongoozler

*Gongoozler* (British slang): someone who idly watches the boats go by on a canal. This one
watches backtests.

Write a backtest as one Python file, a plain script or a marimo notebook, with its dependencies in
its header (PEP 723). Run it with `uv run`, or with `gongoozler run` to watch it, here or in a box
on a server.
- **Every run reports as it goes:** its stage, how far through the simulation it is, its P&L and
  its memory, at no measurable cost to the run.
- **The truth about how it ended comes from outside it:** its exit, its exact memory and any
  out-of-memory kill, the stages it was in, and nothing left running. A run dies with its
  supervisor, and is marked lost.
- **Results are data:** the same run writes the same bytes, so `gongoozler rerun` proves a result
  reproduces, and `gongoozler diff` shows what changed.
- **Any data library feeds it:** the library defines its reads, then hands the backtester a feed
  stream. A feed can be written once to a Nautilus catalog and streamed in Rust by every run after.
- **Boxes:** a run that needs more than the desktop has goes to a server, which runs it in a box of
  its own, watched the same way.
- **A fixed income backtester is built in** (`engine/`, `gongoozler[fi]`): bonds, loans and deals
  defined by their terms, a ledger that books every coupon, call, default and trade exactly, in
  Rust with Polars and Nautilus, reporting its progress to the watcher from its own loops.

## Quick start (Linux, or WSL on Windows)

You need [uv](https://docs.astral.sh/uv/), Go 1.25 or later, and, for the engine, Rust
([rustup](https://rustup.rs) installs the version `engine/rust-toolchain.toml` pins).

```sh
make build && cp bin/gongoozler ~/.local/bin/
gongoozler run examples/ema_cross.py                  # live in a terminal; plain lines when piped
gongoozler run --param days=5 examples/ema_cross.py   # set a parameter the file declares
gongoozler run --json examples/ema_cross.py           # every event as JSON, for agents
gongoozler sweep --param fast=5,10,20 --rank 'pnls.USD.PnL (total)' examples/ema_cross.py
gongoozler ls                                         # runs, here and remote
gongoozler attach RUN; gongoozler kill RUN            # watch, or stop, a run that is going
gongoozler rerun RUN; gongoozler diff RUN RUN         # reproduce a run; compare two
uv run examples/ema_cross.py                          # the same file with no CLI: progress still prints
gongoozler run examples/fi_bonds.py                   # the fixed income engine: bonds from terms, results kept
gongoozler sweep --rank run.total_return examples/fi_bonds.py
```

Each run keeps `meta.json`, `events.jsonl`, `output.log`, `artifacts/` (orders, fills,
positions, account states, returns, statistics, a series and a manifest) and `source/` under
`~/.local/state/gongoozler/runs/ID/`. The first run of a file locks its dependencies beside it
(`FILE.lock`).

**A server and workers** (one machine, or many):

```sh
GONGOOZLER_TOKEN=secret gongoozler server --listen 0.0.0.0:8150
GONGOOZLER_TOKEN=secret gongoozler worker --server http://HOST:8150 --docker --image gongoozler   # docker build -t gongoozler .
export GONGOOZLER_SERVER=http://HOST:8150 GONGOOZLER_TOKEN=secret
gongoozler run --remote examples/in_a_box.py          # or let a file's [tool.gongoozler] memory decide
```

## The fixed income engine

`engine/` is a generic backtester for fixed income, reviewed against an institutional platform brief
([review/2026-11-03-engine.md](review/2026-11-03-engine.md)) and built out to it:
- **Instruments from their terms** (`fi.Bond`, `fi.bonds` from a terms table): day counts
  (ACT/360, ACT/365F, ACT/ACT ISDA and ICMA, the 30/360 family), calendars from declared holidays,
  schedules with stubs and ex-coupon periods, fixed, stepped, floating (in advance, compounded or
  averaged in arrears) and inflation-linked coupons, amortisation, calls, puts and extensions.
- **Analytics:** prices from curves and yields, yield and Z-spread solvers, durations, and
  bump-and-reprice sensitivities that carry their definition.
- **A ledger that cannot be fooled:** bitemporal data seen only as it was known, P&L by role
  (income, price, principal, other, financing, currency, costs) checked against NAV every step,
  cash in exact decimals, trade and settlement dates, receivables, a coupon gone ex paid to its
  seller, financing agreements and margin, holding transitions (exchanges, conversions, delayed
  recoveries), and a step order that is declared rather than assumed.
- **Deals:** CLO and ABS waterfalls, tests, triggers and manager counterfactuals as books.
- **Rust inside, Python outside:** frames from anywhere through Arrow into Polars, Nautilus's
  order book and statistics, runs and sweeps with the GIL released.

```python
import fi_engine as fi
import gongoozler.fi as gzfi

ws = fi.Workspace()
ws.series("marks", marks, entity="isin", kind="bond", time="date", values={"clean": "price"})
bonds = fi.bonds(terms)                                       # a terms table, one row a bond
records = ws.records(ws.debt(bonds, fi.valuation.clean("marks.clean")), ws.calendar("marks"))
run = gzfi.run(ws, fi.Strategy(fi.Decision.top_n("marks.signal", 50)), records)   # results kept
```

The engine's own README is [engine/README.md](engine/README.md); its spec is
[engine/docs/spec.md](engine/docs/spec.md).

## A run file

```python
# /// script
# requires-python = ">=3.13"
# dependencies = ["gongoozler", "nautilus_trader==2.0.0rc8", "your-data-library"]
# [tool.gongoozler]
# memory = "48GiB"
# sweep = { fast = [10, 20], slow = [50, 100] }
# ///
import gongoozler as gz
from gongoozler import nautilus as gzn

p = gz.params(fast=10, slow=50)                 # the CLI or a sweep can set these
engine = ...                                    # Nautilus's full API, as usual
feed = your_data_library.quotes_feed(...)       # a Feed: batches sorted by ts_init
gzn.run(engine, feed)                           # streams it, reports progress, keeps the results
```

- **What a run can report:** `gz.stage`, `gz.progress`, `gz.metric`, `gz.log`, `gz.artifact`
  and `gz.emit`.
- **The specs:** the run file ([spec/run-file.md](spec/run-file.md)), the feed contract
  ([spec/feeds.md](spec/feeds.md)), the events ([spec/protocol.md](spec/protocol.md),
  [spec/ring.md](spec/ring.md)), the supervisor ([spec/supervisor.md](spec/supervisor.md)),
  results ([spec/results.md](spec/results.md)) and the server ([spec/server.md](spec/server.md)).
  The design is [spec/design.md](spec/design.md).

## The repo

| Path | What |
| --- | --- |
| `python/` | The `gongoozler` Python library and its tests |
| `engine/` | The fixed income engine: Rust crates (terms, kernel, contracts, research, I/O, execution, metrics, the ring writer), its Python package `fi_engine`, examples and tests |
| `cmd/gongoozler`, `internal/` | The CLI, the supervisor, the server and the worker (Go), and their tests |
| `examples/` | Run files |
| `spec/` | The design, and the normative specs |
| `plan/` | Slices, each with its evidence |
| `kb/` | What we rely on from Nautilus, uv, Linux and systemd, marimo and WSL, and the measurements |
| `review/` | The 3 Nov 2026 reviews (the watcher's and the engine's), their reproductions, and how each finding was resolved |
| `tools/bench/` | The budgets (`make bench`) |
| `tools/spikes/` | The measurement scripts (locked PEP 723 files) |
| `Dockerfile`, `tools/e2e-docker.sh` | The box image, and the end-to-end run with Docker |

`make check` runs everything: gofmt and ruff format, go vet, golangci-lint, ruff, mypy, the
engine's rustfmt, clippy, Rust and Python tests, the Go tests under the race detector, and the
Python tests against Nautilus 2.0.0rc8 and the engine. `make bench` holds the budgets; `make fuzz`
fuzzes what reads a run's bytes; CI runs all three, the engine's budgets at full scale, and the
Docker end-to-end.

## License

[MIT](LICENSE).
