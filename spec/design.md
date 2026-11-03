# gongoozler: the design

*Gongoozler* (British slang) is someone who idly watches the boats go by on a canal. This one
watches backtests.

First written 3 Nov 2026 in the fi-engine repo as "the Lab", it moved here the same day. It
records Ashley's decisions:
1. **A run is one Python file:** a plain script or a marimo notebook, with its dependencies in its
   header (PEP 723), run by `uv run`.
2. **The file uses the full libraries:** any data library, and the backtesters (Nautilus,
   bt-research, fi-engine) with their full Python APIs.
3. **It runs where the person is.** A file is published to a server only for capacity: memory,
   mostly, or a cloud harness that cannot hold the run. The server then runs it in a box of its
   own.
4. **Runtime progress is first-class,** and it must be "VERY VERY SCALABLE AND WELL IMPLEMENTED".
5. **Where it runs does not matter:** "I'll docker it up and run it anywhere I want." Local users
   use WSL.
6. **"The data library is not relevant to you… any data library should define reads and then
   define the feed streams into the backtester."**
7. **It is its own repo, with its own weird random name.**

The normative pages are:
- [protocol.md](protocol.md): the events;
- [ring.md](ring.md): how they leave a run's process;
- [supervisor.md](supervisor.md): what is known about a run, and how;
- [feeds.md](feeds.md): the feed contract;
- [results.md](results.md): what a run keeps;
- [run-file.md](run-file.md): the file, its header, its parameters, placement and sweeps;
- [server.md](server.md): the server, workers and boxes.

The dependency facts are in [../kb](../kb), the plan in [../plan](../plan/README.md), and the
review that reshaped the code in [../review/2026-11-03.md](../review/2026-11-03.md).

## The vision

Write a backtest as one Python file. Run it here, or in a box sized for it, with one command.
Watch it run from a terminal, a browser or an agent.

1. **One file, full power.** Nothing hides behind a form.
2. **Here or in a box, the same result.** The same file, the same lock and the same environment.
3. **Always visible.** Wherever it runs, every run reports as it goes:
   - its stage;
   - how far through the simulation it is;
   - its P&L;
   - its memory.

   And how it ended, even when it was killed.
4. **Data from anywhere.** A data library defines its reads, then hands the backtester feed
   streams. gongoozler checks the streams and drives the engine.

## The parts

```
 run file (script or marimo)  ──uses──▶  gongoozler (Python): events, params, feeds, catalogs, the Nautilus drivers, results
        │ uv run --script                      │ events, into a shared-memory ring
        ▼                                      ▼
 here:      the `gongoozler` CLI: the supervisor runs the file, reads its rings from outside, owns its
            process tree and cgroup, stores and shows every event; sweeps; diff and rerun
 in a box:  `gongoozler server` queues it; `gongoozler worker` runs it with the same supervisor, in its
            own process or a container of its own; events stream back over SSE
```

| Part | Language | What it does |
| --- | --- | --- |
| `python/` (`gongoozler`) | Python, no dependencies | The emitter (`stage`, `progress`, `metric`, `log`, `artifact`, `emit`), `params`, the feed contract, `gongoozler.catalog`, `gongoozler.nautilus` (`run`, `run_node`, `tearsheet`), `gongoozler.results` |
| `cmd/gongoozler` | Go | `run`, `sweep`, `ls`, `events`, `attach`, `kill`, `diff`, `rerun`, `pull`, `server`, `worker`, `version` |
| `internal/supervise` | Go | The supervisor: process tree, cgroup or PSS, rings, event log, artifacts |
| `internal/server`, `internal/worker` | Go | Jobs, leases, blobs, event streams; the process and Docker launchers |
| `Dockerfile` | | The box image: the CLI, uv, Python, the library, a warm cache |
| A web page | TypeScript | The run page, live (slice 3) |

## The run file

`examples/ema_cross.py` is the model, and [run-file.md](run-file.md) the rules:
- a PEP 723 header, its dependencies from any index;
- `[tool.uv]` for reproducibility;
- `[tool.gongoozler]` for its memory, cores, time limit, placement and sweep.

Its body uses Nautilus's full API to build the engine, gets data from a library as a feed, and
hands both to `gongoozler.nautilus.run`.

**It runs with no gongoozler at all.** Plain `uv run file.py` works, and progress then prints to the
terminal. `gongoozler run file.py` adds:
- the supervisor: memory, limits and an exact account of how the run ended, from outside it;
- the event log, results with a manifest, and `diff` and `rerun`;
- a lock on the file's first run, so every run resolves the same packages;
- JSON for agents.

## Runtime progress

How it works is in [protocol.md](protocol.md) and [ring.md](ring.md). The design choices behind
it:
- **The run never waits, and holds nothing anyone else wants.** A call writes into a
  shared-memory ring and returns: no thread, no socket, no system call. The supervisor reads the
  ring from outside the process, so the run's Python lock is never contended (review H4: a sender
  thread had made a strategy 67% slower).
- **Lossless where it matters, coalesced where it doesn't.** Stages, artifacts and errors are
  records, never dropped while the ring has room. Progress and metrics are latest-value slots.
  Logs and other events are rate-limited. Every drop is counted and reported in `run.end`.
- **The truth about a run comes from outside it** ([supervisor.md](supervisor.md)): its exit,
  exact memory and out-of-memory kills from its cgroup, the stages it was in, and what it left
  behind. A run dies with its supervisor and is marked lost.
- **Progress from Nautilus costs nothing.** The driver uses Nautilus's streaming loop and reads
  progress between windows; where batching is not in our hands (`BacktestNode`), an actor with
  about 1,000 timer events per run stays within noise. Measured by `make bench`: within 1%.
- **Simulated time is not wall time.** Progress and the kept series are in simulated time; the
  series lies on an exact simulated-time grid, the same however the data was batched.
- **fi-engine progress** will come from its Rust loops writing slots of the same ring while the run
  holds no Python lock: a core change in fi-engine, in slice 4.

## Data

A data library defines its reads, then its feed streams ([feeds.md](feeds.md)). gongoozler never
reads anyone's data. It:
- checks each stream against the contract, look-ahead included, and fingerprints it;
- merges several streams without changing what the engine replays;
- drives the engine with them;
- writes a stream once to a Nautilus `ParquetDataCatalog`, for runs that stream it in Rust.

Streaming bounds memory: only a window is ever held. How a library authenticates in a box is the
library's and the deployment's business: a box gets the environment its deployment gives it.

## Boxes: Docker, anywhere

Everything runs wherever Docker runs ([server.md](server.md)):
- **`gongoozler server`** keeps files, locks and artifacts as content-addressed blobs, queues runs
  as jobs, stores their events and streams them over SSE (resuming with `Last-Event-ID`). Its
  state is a directory; one server serves many workers.
- **`gongoozler worker`** leases jobs that fit what it has free, renews each lease while the run
  lasts, and runs the job with the same supervisor as a local run: in its own process, or in a
  container of its own through the Docker Engine API, with the job's memory and CPU limits. A
  worker that goes quiet loses its lease, and the job runs again elsewhere.
- **The image** carries Python, uv, the library and a warm uv cache with Nautilus in it.
- **The CLI:** `gongoozler run --remote file.py` publishes the file and follows the run as it
  follows a local one. Without `--remote`, placement follows the file, then
  `GONGOOZLER_PLACEMENT`, then the file's memory against the machine's ([run-file.md](run-file.md)).
  Publishing exists for capacity, not for sharing.
- **Sweeps:** one file, many parameter sets: here across the cores, or on the server, a box each.
- **Later:** Postgres and object storage behind the server, replicas, and a Kubernetes launcher
  (Kueue, Indexed Jobs) in place of Docker: adapters, not a different system (slice 5).

## Options weighed

| Option | Verdict | Why |
| --- | --- | --- |
| Ray Jobs, Modal, Coiled | The experience to match | They run a file with its dependencies in a box, which is what is wanted. But a Ray cluster shares its nodes between jobs, and the hosted ones run outside the firm |
| Jupyter with papermill | No | marimo's `.py` files diff, lock and run as scripts |
| OpenTelemetry for progress | For operations only | It is the right pipe for the platform's own traces and metrics, but it has no per-run live stream with replay |
| WebSockets | No | Progress flows one way. SSE passes proxies and resumes with `Last-Event-ID` |
| An event server inside each box | No | Viewers would connect to boxes, which come and go |
| Nautilus's Redis bus for progress, now | Later | Upstream documents external egress for live nodes, not for Python backtests. The streaming loop covers progress today |
| Nautilus-Web-Interface | Ideas only | It runs backtests inside its API process, one at a time behind a global flag, on fixed strategies |

## Questions still open for Ashley

Decided on 3 Nov 2026: the licence is MIT, and the repo is public on GitHub
(`cottrellashley/gongoozler`).

1. **Docker on Windows:** Docker Desktop with WSL integration, or Docker inside WSL? This decides
   how the local quick start for boxes reads.
2. **Docker on this machine:** its user is not in the `docker` group, so the Docker path
   (`make e2e-docker`) has run only against a fake Engine API here. Joining the group is Ashley's
   call; CI runs it for real.
3. **Where the library is published.** Files name `gongoozler`; a box resolves it from the wheel in
   its image, and a desktop from a path source. Anywhere else needs it on an index: PyPI, or the
   firm's.
