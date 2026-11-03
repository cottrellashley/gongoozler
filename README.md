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

## Quick start (Linux, or WSL on Windows)

You need [uv](https://docs.astral.sh/uv/) and Go 1.25 or later.

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
| `cmd/gongoozler`, `internal/` | The CLI, the supervisor, the server and the worker (Go), and their tests |
| `examples/` | Run files |
| `spec/` | The design, and the normative specs |
| `plan/` | Slices, each with its evidence |
| `kb/` | What we rely on from Nautilus, uv, Linux and systemd, marimo and WSL, and the measurements |
| `review/` | The 3 Nov 2026 review, its reproductions, and how each finding was resolved |
| `tools/bench/` | The budgets (`make bench`) |
| `tools/spikes/` | The measurement scripts (locked PEP 723 files) |
| `Dockerfile`, `tools/e2e-docker.sh` | The box image, and the end-to-end run with Docker |

`make check` runs everything: gofmt and ruff format, go vet, golangci-lint, ruff, mypy, the Go
tests under the race detector, and the Python tests against Nautilus 2.0.0rc8. `make bench` holds
the budgets; `make fuzz` fuzzes what reads a run's bytes; CI runs all three, and the Docker
end-to-end.

## License

[MIT](LICENSE).
