# The server, workers and boxes

This page is normative. `internal/server`, `internal/client`, `internal/worker` and
`internal/api` implement it. Tests: `internal/server/server_test.go`,
`internal/worker/worker_test.go` (with a fake Docker Engine API), and the CLI's
`TestAFileRunOnTheServerKeepsTheSameResultsAsHere`, `TestARemoteRunIsCancelledWithKill` and
`TestAFileDeclaringMoreMemoryThanThisMachineHasGoesToTheServer`. `tools/e2e-docker.sh` runs the
whole path with Docker.

```
 gongoozler run --remote ──HTTP──▶ gongoozler server ◀──HTTP── gongoozler worker ──Docker API──▶ a box per run
   uploads, submits,                keeps blobs and runs,          leases jobs, renews               gongoozler run --json
   follows (SSE), pulls             queues jobs, streams           leases, pushes events,            (the same supervisor),
                                    events (SSE)                   uploads artifacts                 its cgroup the container's
```

The server holds no run. A worker runs it, with the same supervisor as a local run, in the
worker's process or in a container of its own. Adding workers adds capacity.

## The API

JSON over HTTP. With `GONGOOZLER_TOKEN` set on the server, every request carries
`Authorization: Bearer TOKEN` (compared in constant time); without it, the server warns when it
listens beyond this machine.

| Request | What |
| --- | --- |
| `GET /v1/health` | The version, and how many runs are queued, running and done |
| `PUT /v1/blobs/SHA256` | Stores a blob. Refused unless its content hashes to its name |
| `GET`/`HEAD /v1/blobs/SHA256` | A blob |
| `POST /v1/runs` | Submits a run (`api.Spec`): the file's name and blob, its lock's blob, arguments, parameters, `memory`, `cpus`, `timeout`, its sweep. Refused unless the blobs are stored. Returns its record |
| `GET /v1/runs`, `GET /v1/runs/ID` | Run records (`runs.Meta`, as for a local run), with `worker` and `attempt` |
| `POST /v1/runs/ID/cancel` | A queued run ends at once (`cancelled`); a running one is cancelled through its worker |
| `GET /v1/runs/ID/events` | Server-sent events: each stored event as `id: N` and `data: EVENT`, after `Last-Event-ID` (or `?after=N`), following the run until it ends, then `event: end` with its status. A ping every 15 s. `?follow=0` gives the events stored now as JSON lines |
| `GET /v1/runs/ID/artifacts`, `…/artifacts/NAME` | What the run kept, by name and blob |

**For workers**, each carrying its lease in `X-Gongoozler-Lease`:

| Request | What |
| --- | --- |
| `POST /v1/lease` | `api.Offer` (the worker's name, the memory and CPUs it has free, how long to wait): the oldest queued job that fits, or 204 after the wait (at most a minute) |
| `POST /v1/runs/ID/heartbeat` | Renews the lease; the answer says whether to cancel |
| `POST /v1/runs/ID/events` | The run's events as JSON lines, as its supervisor numbered them |
| `POST /v1/runs/ID/complete` | The run's record and its artifacts (already uploaded as blobs) |
| `POST /v1/runs/ID/release` | Gives the job back (a worker shutting down) |

A request with a lease no longer held gets 409: the worker stops the run and reports nothing.

## Jobs and leases

- **A job fits** a worker that offers at least its `memory` and `cpus` (a job declaring neither
  fits any).
- **A lease lasts** `--ttl` (default 15 s) past the worker's last heartbeat or push. A worker
  renews every third of it.
- **A lease that expires** puts the job back in the queue (`job.expired`, `job.requeued`), up to
  `--attempts` (default 3); after that the run is `lost`. A job given back is requeued the same
  way. A cancelled job whose worker goes quiet ends `cancelled`.
- **Each event is stored once, from the current lease only.** The server numbers events `n` in
  the order it stores them, keeps the supervisor's number as `wn`, and skips any it has had (a push
  retried after a timeout). A retry's events carry `attempt`.
- **A server restarted** puts its queued and running jobs back in the queue.

The server's own events: `job.queued`, `job.leased` (`worker`, `attempt`), `job.expired`,
`job.requeued` (`reason`), `job.released`, `job.cancel`, and `run.end` for runs it ends itself.
The live view shows them.

**Evidence:** a job goes to the first worker it fits and a 64 GiB job never to an 8 GiB worker; a
retried push is stored once; an expired lease runs the job again, then the run is lost; the old
lease is refused; heartbeats keep a lease and carry a cancellation; a follower resumes where it
broke off and stops at the end; blobs are checked against their names; a token is required; a
restarted server requeues its jobs (`server_test.go`). A worker that loses its lease stops the run
and reports nothing; one shutting down gives its job back, and another worker runs it
(`worker_test.go`).

## On disk

```
DIR/blobs/SHA256           files, locks and artifacts, by content
DIR/runs/ID/meta.json      the run's record
DIR/runs/ID/spec.json      what was submitted
DIR/runs/ID/events.jsonl   its events, numbered n
DIR/runs/ID/artifacts.json what it kept
```

`DIR` is `--data` (default `STATE/server`). Event logs are synced as each run ends.

## Workers

`gongoozler worker --server URL` takes jobs while it can: `--slots` at once (default 1), within
`--memory` (default: what is available) and `--cpus` (default: all). For each job it fetches the
file and lock, runs it with a launcher, pushes its events as they are stored (in batches, every
100 ms; a million can queue while the server is away, progress and output dropped first, and
counted), uploads its artifacts, and completes it. Stopped (SIGTERM), it cancels its runs and
gives their jobs back.

**The process launcher** (the default) runs the job with the supervisor in the worker's own
process: on the worker's machine, with a cgroup where one can be made, or in the worker's
container.

**The Docker launcher** (`--docker --image IMAGE`) runs each job in a container of its own,
through the Docker Engine API (its Unix socket, or `DOCKER_HOST`):
- the container runs `gongoozler run --json --local --no-lock --id ID --params-json … /job/FILE`
  (the entrypoint set by the worker, whatever the image's): the same supervisor as a local run,
  accounting by the container's cgroup;
- the API version is the daemon's own, from `/_ping`, as Docker's client negotiates it: daemons
  refuse versions older than their minimum;
- its limits are the job's: `Memory` (with no swap), `NanoCpus`, and an init (`Init: true`);
- the file and lock go in, and the run's record and artifacts come out, through the API's
  archives, so the worker shares no filesystem with its boxes;
- its events come back on its stdout, read from the API's multiplexed log stream;
- a cancellation sends SIGUSR1 (as `gongoozler kill` does here), then SIGKILL after a grace
  period;
- if the box ends before its supervisor records the run (the container killed for memory, say),
  the worker records it: `oom` when Docker says `OOMKilled`, with what the box said on stderr.

The image (`Dockerfile`) has the CLI, uv, Python 3.13, the library as a wheel (a file names
`gongoozler` and it resolves there), and a warm uv cache with Nautilus in it. Docker could not be
run where this was built (its user is not in the `docker` group), so the Docker launcher is tested
against a fake Engine API, and `tools/e2e-docker.sh` (`make e2e-docker`, and CI) runs it for real.

## The CLI

- `gongoozler run --remote FILE` uploads the file and its lock, submits, and follows the run as it
  would a local one, mirroring its events into `STATE/runs/ID/` and fetching its artifacts at the
  end; it exits with the run's code. The first interrupt cancels the run on the server; a second
  stops watching. `--detach` submits and prints the ID.
- `attach`, `events [--follow]`, `kill`, `pull` and `ls` treat a remote run as they do a local one
  (`ls --remote` lists the server's runs). `diff` and `rerun` compare a remote run with any other.
