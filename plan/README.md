# Plan

Each slice ends with its evidence: tests that would fail without it, or measurements kept in
[../kb/measurements.md](../kb/measurements.md). The 3 Nov 2026 review
([../review/2026-11-03.md](../review/2026-11-03.md)) reshaped slices 1 and 2; its resolution
section maps each finding to its fix and test.

## 1. Local, end to end: built

**Built:**
- `gongoozler` (Python, no dependencies): the emitter writing into a shared-memory ring, `params`,
  the feed contract (look-ahead guard, declarations, fingerprints, vectorised checks), catalog
  feeds, `gongoozler.nautilus.run` and `run_node`, results with a manifest, `tearsheet`.
- The supervisor (Go): process tree, cgroup or PSS, a dead man's switch, rings read from outside,
  the event log, artifacts copied and hashed outside the run.
- The CLI: `run` (live, plain or JSON; lock on first run; placement), `sweep` (across cores,
  ranked), `ls`, `events --follow`, `attach`, `kill [--force]`, `diff`, `rerun`, `pull`.

**Evidence:**
- **Python: 264 tests**, against Nautilus 2.0.0rc8. The event order across records and slots, no
  value able to break a record, counted drops, forks, threads, crash rings; the feed contract and
  200 randomised merges; a streamed run equals one run; results byte-identical across processes
  and batch sizes; a catalog run equals a streamed run, every file.
- **Go: every package under `-race`**, with fuzzing of the decoder, the ring and the header. A run
  dead within a second of its supervisor (with a cgroup, whatever it ignores) and listed lost;
  exact memory for forked workers; an exact out-of-memory kill; one interrupt through real uv;
  no leftovers; events in the log within 250 ms at p99; a stalled terminal never holding a run.
- **Budgets (`make bench`, in CI):** telemetry within 1% of a run with events off; a progress
  update about 2 µs; ingest from a catalog as fast as Nautilus's own engine given the same quotes
  from memory (1.25M quotes a second here).

**Not yet:** exercised inside a WSL distribution itself (see `kb/wsl.md`).

## 2. Boxes, with Docker: built

**Built:** `gongoozler server` (blobs, runs, leased jobs with heartbeats, expiry and retry, SSE
with `Last-Event-ID`, cancellation, artifacts, a bearer token); `gongoozler worker` (the process
launcher, and the Docker launcher over the Engine API); `run --remote`, placement from
`[tool.gongoozler]`; the box image (`Dockerfile`); `tools/e2e-docker.sh`.

**Evidence:**
- the same file gives byte-identical results here and through the server and a worker
  (`TestAFileRunOnTheServerKeepsTheSameResultsAsHere`);
- an expired lease runs the job again, then the run is lost; a retried push is stored once; a
  restarted server requeues its jobs; a worker shutting down gives its job back and another runs
  it; a worker that loses its lease stops the run;
- the Docker launcher against a fake Engine API: the container's command and limits, files in and
  results out through archives, a cancellation as SIGUSR1, an out-of-memory kill reported.

**Not yet:** `make e2e-docker` run on this machine (Docker is not available to its user). CI runs
it.

## 3. The run page

**Build:** the live page over SSE (the server's stream, and the same from the CLI as
`gongoozler ui`). Its tabs come from the events and artifacts: stages, progress, metrics, memory,
output, the kept series and statistics, Nautilus's tearsheet.

**Evidence:**
- a viewer joining late sees everything;
- a slow viewer slows no run.

## 4. fi-engine

**Build:**
- progress written by fi-engine's Rust loops (ledger steps, sweep variants, worlds) into slots of
  the same ring (spec/ring.md), while the run holds no Python lock;
- `gongoozler.fi`.

**Evidence:** fi-engine's example E1 as a run file, with progress, at no measurable cost.

## 5. Scale

**Build:** Postgres and object storage behind the server, replicas, and a Kubernetes launcher
(Kueue for quotas, Indexed Jobs for sweeps).

**Evidence:** a load test: 1,000 concurrent runs and 5,000 viewers, events under a second at the
99th percentile, and no viewer slowing a run.

## 6. Reports

**Build:** marimo notebooks exported as the run's report (`marimo export html`), fixed income
tearsheet panels, and sweep leaderboards from the sweeps' records.
