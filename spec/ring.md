# The ring: how a run's events leave its process

This page is normative. The writer is `python/src/gongoozler/_ring.py` (with the emitter in
`_events.py`), the reader `internal/ring` (with `internal/supervise/telemetry.go`). Their tests
check each other: `python/tests/test_events.py` reads rings back with an independent reader
(`tests/ringread.py`), and `internal/ring` reads rings that Python wrote. `FuzzRing` and
`FuzzOpen` hold the reader to any bytes at all.

## Why a ring

Telemetry used to leave the run through a socket, written by a thread in the run's process. That
thread competed with the strategy for Python's lock, and one bad value killed it, and with it the
run's whole record (review C1, H4). Now:
- **a call writes into shared memory and returns:** no thread, no socket, no system call;
- **the supervisor reads it from outside the process**, so the run's lock is never wanted;
- **what was written survives the process:** after a crash, the supervisor drains the file.

## The file

Each process of a run makes its own ring, the first time it emits: `PID-RANDOM.ring` (mode 0600)
in `GONGOOZLER_RING`, a directory the supervisor makes in `/dev/shm` (private to its user) when it
can, and in the run's directory otherwise. A forked child makes its own. All integers are
little-endian.

```
 0      4096                      4096 + data_size             + slot_count × 256
 │header│ records (a ring of data_size bytes) │ slots: the latest value of each progress key and metric │
```

| Offset | Type | Field |
| --- | --- | --- |
| 0 | 8 bytes | magic `GZRING01`, written last: a reader that sees it sees a whole header |
| 8 | u32 | version: 1 |
| 12 | u32 | header size: 4096 |
| 16 | u64 | `data_size`: a power of two, at least 64 (default 8 MiB) |
| 24 | u64 | the slots' offset: 4096 + `data_size` |
| 32 | u32 | `slot_count` (default 1,024) |
| 36 | u32 | slot size: 256 |
| 40 | u32 | the writer's PID |
| 48 | u64 | when it was made, in nanoseconds since the epoch |
| 64 | u64 | `head`: bytes ever written. The writer's alone; on its own cache line |
| 128 | u64 | `tail`: bytes ever consumed. The reader's alone; on its own cache line |
| 192 | u64 | records dropped: the ring was full |
| 200 | u64 | events cut down to fit (or refused, at over a quarter of the ring) |
| 208 | u64 | events refused by a rate limit |
| 216 | u64 | 1 once the writer closed cleanly |
| 224 | u64 | slots in use |
| 232 | u64 | updates refused: every slot was taken |

A reader refuses a header that breaks any rule above, and waits for one without its magic.

## Records: what is ordered

A record is `[u32 length][u32 crc32 of the payload][payload]`, padded to 8 bytes; the payload is
one event as JSON (spec/protocol.md). A record that would cross the end of the ring is preceded
by a wrap marker, a length of `0xFFFFFFFF`, and starts again at 0.

**The writer** never waits. It writes the payload, then the length and crc, then advances `head`.
If the record does not fit in what the reader has not consumed (`data_size − (head − tail)`), it
drops it and counts it.

**The reader** (`Drain`) consumes from `tail` up to `head`, verifying each record's crc before it
hands it on, and stores `tail` after each one. A record whose crc does not match yet (its bytes
not all visible) stops the drain, and is read on the next poll. It never consumes more than
`data_size` bytes in one call, and counts what it has consumed rather than comparing positions,
so a header with counters at their limits cannot hold a reader forever (both were found by
fuzzing). A record that never verifies, from a writer that is gone, is abandoned (`Skip`) and
counted, so a run ends with its telemetry marked incomplete, never silently.

## Slots: what is coalesced

Progress and metrics change far faster than anyone reads them, so each key keeps only its latest
value, in a slot of its own (the first `slot_count` keys; later ones are refused and counted).

| Offset in the slot | Type | Field |
| --- | --- | --- |
| 0 | u64 | `seq`: odd while being written, even when stable |
| 8 | u32 | crc32 of the bytes from 12 to the end of the key |
| 12 | u8 | kind: 1 progress, 2 metric |
| 13 | u8 | flags: 1 `b`, 2 `c`, 4 `d`, 8 `e` present; 16 `a` is null; 32 `a` is an integer |
| 14 | u16 | key length (at most 176 bytes) |
| 16 | f64 | `a`: progress `done`, or the metric's `value` |
| 24 | f64 | `b`: progress `total` |
| 32 | i64 | `c`: progress `sim`, or the metric's `at` (simulated nanoseconds) |
| 40 | f64 | `d`: progress `rate` |
| 48 | f64 | `e`: progress `eta` |
| 56 | f64 | `t`: wall time of the update |
| 64 | u64 | `after`: the `seq` of the last record written before this update |
| 72 | 8 bytes | reserved |
| 80 | bytes | the key |

**The writer** makes `seq` odd, writes the body and its crc, then makes `seq` even. **The reader**
copies a slot whose `seq` is even and has moved, checks `seq` is unchanged after the copy and
the crc matches, and otherwise reads it next time. A progress key's unit travels once, as an
ordered `progress.unit` event, when it changes.

## Order

Events from one process reach the log in the order the program made them:
1. **A slot value is shown only after the record before it.** Each update carries `after`; the
   reader holds an update back until it has drained that far. Each poll drains records first,
   then reads slots.
2. **A slot value made before a record goes ahead of it.** Before writing an ordered event, the
   writer writes each slot changed since the last record as a record of its own, carrying the
   slot's index and `seq` (`slot`, `sseq`), so the reader shows that value in order and skips it
   when it reads the slot.

So progress never appears before the stage it belongs to, nor after the end of it
(`test_progress_made_before_an_ordered_event_is_written_ahead_of_it`,
`TestARunsEventsAreStoredInProgramOrder`). Each slot is shown at most ten times a second.

## What it costs, and what it loses

Measured by `make bench` (`kb/measurements.md`): a progress update costs about 2 µs and an
ordered event about 4 µs in the run's process, and a backtest with progress and portfolio
snapshots every 250 ms runs within 1% of the same backtest with events off.

Nothing is lost silently. A full ring, a cut or refused event, a rate limit, a full slot table or
an abandoned record is counted in the ring's header, and `run.end` reports the totals
(`telemetry`) and says the telemetry was incomplete.

## The same ring from Rust

The format is language-neutral by design: fi-engine's Rust loops (slice 4) are to write progress
into slots of the same ring directly, while the run holds no Python lock.
