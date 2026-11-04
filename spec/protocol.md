# The event protocol: gongoozler-events/1

How a run reports itself, and what its supervisor, a server and a worker add. This page is
normative: the Python library (`python/src/gongoozler/_events.py`) and the Go side
(`internal/events`, `internal/supervise`, `internal/server`) implement it, and their tests check
it. How events leave a run's process is in [ring.md](ring.md).

## The envelope

An event is one JSON object, at most 1 MiB as a run sends it (4 MiB as stored). Every event has:

| Field | Type | Meaning |
| --- | --- | --- |
| `v` | integer | The protocol's major version: `1` |
| `kind` | string | What the event is (below). Unknown kinds pass through unchanged |
| `t` | number | Wall time at which it was made, in seconds since the epoch |
| `seq` | integer | The producing process's sequence number, strictly increasing |

A run's events also carry `pid` (the process that made it) and `run` (its ID, from
`GONGOOZLER_RUN_ID`). The supervisor adds `rt` (when it read the event) and `n` (its number in the
run's log: 1, 2, 3 …, in the order stored), and marks its own events `"src": "cli"`. A server
renumbers `n` in its own order and keeps the supervisor's as `wn` (spec/server.md); its own events
are `"src": "server"`, and a worker's `"src": "worker"`.

A record that is not JSON, lacks the envelope, carries another major version, or has data after
the object is not fatal: the supervisor counts it, stores a `cli.protocol` event saying why (the
first 20), and goes on. `FuzzDecode` holds the decoder to any bytes.

## What a run sends

| Kind | Fields | Delivery |
| --- | --- | --- |
| `hello` | `lib` (the library's version), `pid`, `ppid`, `python`, `script`, `argc`; `argv` only with `GONGOOZLER_ARGV=1` (arguments may hold secrets). Another writer in the process names itself: `producer` (`"fi-engine"`) and its version | First, from each ring |
| `stage` | `name`, `phase` (`begin` or `end`), `depth`; at the end also `elapsed` (seconds), `ok`, and `error` if it failed | Ordered |
| `progress` | `key`, `done`; optionally `total`, `unit` (`ns`, `items`, …), `sim` (simulated time, nanoseconds since the epoch), `rate` (units a second), `eta` (seconds) | The latest value per `key` (a slot) |
| `progress.unit` | `key`, `unit` | Ordered, once per change; readers merge it into `progress` |
| `metric` | `name`, `value`; optionally `at` (simulated nanoseconds) | The latest value per `name` (a slot) |
| `log` | `level`, `msg` | Ordered; 200 a second, bursts of 2,000; the rest counted |
| `artifact.declare` | `name`, `path`, `bytes` | Ordered; the supervisor copies and hashes the file |
| `params` | `values` | Ordered |
| `error` | `type`, `msg`, `where` (`file:line`), and `thread` for an uncaught exception in another thread | Ordered |
| any other | anything (`gz.emit(kind, **fields)`) | Ordered; 1,000 a second per kind, bursts of 10,000; the rest counted |

`hello`, `stage`, `error`, `artifact.declare`, `params` and `progress.unit` are never limited.
`progress`, `metric`, `progress.unit`, `hello` and `artifact.declare` cannot be sent with `emit`.

**No value a run passes can break its record** (review C1, C2). Before an event is encoded:
- non-finite numbers become `null`, and keys become strings;
- strings over 64 Ki characters are cut, with how much was cut; lists and objects over 10,000
  items, and nesting over 8 deep, are cut too;
- a value that is none of JSON's becomes its text, or `<Type: str() raised …>`;
- an event still over 1 MiB is replaced by a stub of its envelope with `truncated: true` and its
  size.

Every cut is counted, and `run.end` reports it.

**The rules that let it scale:**
- **The run never waits.** A call writes into the ring and returns: no thread, no lock wanted by
  anyone else, no system call ([ring.md](ring.md)). About 2 µs a progress update.
- **Order is kept where it matters.** Progress never appears before the stage it belongs to, nor
  after its end ([ring.md](ring.md), "Order").
- **Nothing is lost silently.** Every drop, cut or refusal is counted, and reported in `run.end`.
- **Forks and threads report safely.** A forked child makes its own ring, and its own `hello`;
  threads share their process's emitter under its lock. A second writer in one process (the
  engine's Rust code) makes a ring of its own too: each ring is a producer, with its own `seq`.

## Where events go

Under a supervisor, into a ring in `GONGOOZLER_RING`. With none, `GONGOOZLER_EVENTS` says where:

| Value | Destination |
| --- | --- |
| `stderr` or unset | A plain view in the terminal, so `uv run file.py` works with no CLI |
| `file:PATH` | Every ordered event, and each key's last progress and metric, appended as JSON lines |
| `off` | Nowhere |

If a ring cannot be made, the library says so on stderr and prints to the terminal instead.

## What the supervisor adds

| Kind | Fields |
| --- | --- |
| `run.start` | `id`, `file`, `sha256`, `lock_sha256`, `args`, `params`, `launcher`, `host`, `cwd`, `pid`, `version` |
| `resource` | `memory` and `peak` (bytes), `available` (bytes), `procs`, `source` (`cgroup` or `pss`); every second |
| `output` | `stream` (`stdout` or `stderr`), `line`; the first 10,000 lines (all go to `output.log`) |
| `artifact` | `name`, `source`, and `path`, `bytes`, `sha256`; or `error` |
| `cli.protocol` | `error`, `pid` |
| `cli.version` | `error`: the run's library is another version than the supervisor |
| `run.end` | `id`, `status`, `exit_code`, `signal`, `wall`, `peak`, `memory_source`, `oom_kills`, `leftovers`, `telemetry` (its counts, and `complete`), `protocol_errors`, `open_stages`, `startup` (seconds until the run's first event), `note` |

What the statuses mean, and how each is established, is in [supervisor.md](supervisor.md).

**Order between events and printed output is approximate.** They travel by separate paths (the
ring, the pipes), so a `print` can arrive a moment before the stage event that preceded it. Each
event's `t` records when it was made, and `rt` when it was read.

## Compatibility

There is one server and one set of clients, released together. A change that a reader of
version 1 would misread raises `v`, and readers refuse other versions with a `cli.protocol`
event. New kinds and new fields do not raise `v`: readers pass through what they do not know. A
run whose library is another version than its supervisor is flagged (`cli.version`), and runs.
