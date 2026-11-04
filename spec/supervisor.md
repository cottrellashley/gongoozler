# The supervisor: the source of truth about a run

This page is normative. `internal/supervise` implements it; `gongoozler run`, `gongoozler sweep`
and a worker's box all run files through it, so a run is supervised the same way here and in a
box. Its tests are `internal/supervise/supervise_test.go`, and the CLI's in
`cmd/gongoozler/main_test.go`.

A run cannot be trusted to report its own end: it may be killed, crash, or never start. So what
is known about a run comes from outside it: its process tree, its memory, its exit, and its
events, read from rings it cannot block (spec/ring.md).

## What it guarantees

| Guarantee | How | Evidence |
| --- | --- | --- |
| **No orphans.** When `run` returns, nothing the run started is left | The run starts in a process group of its own, and in a cgroup of its own where one can be made. The supervisor is a child subreaper, so orphans come back to it. Once the leader exits, what is left gets SIGTERM, then SIGKILL after a grace period (2 s), and is named in `run.end` (`leftovers`) | `TestProcessesTheRunStartedDoNotOutliveIt` |
| **A run dies with its supervisor, within a second, and is marked lost** | The run's leader has a parent-death signal (SIGTERM), set from an OS thread that lives until the run ends (Linux sends it when the starting *thread* ends). In a cgroup, the scope's anchor is a dead man's switch: its own parent-death signal makes it write `cgroup.kill`, which ends every process in the cgroup whatever signals they ignore. `meta.json` is written as the run starts, so a reader can tell | `TestARunDiesWithinASecondOfItsSupervisor` (with and without a cgroup); `TestARunWhoseCLIIsKilledOutrightDiesWithinASecondAndIsListedLost` |
| **Memory is exact, never a sum of RSS** | In a cgroup: `memory.current` every half second, `memory.peak`, and `memory.events`' `oom_kill`. Without one: the run's processes' summed PSS (`smaps_rollup`) every second, which counts memory shared by forked workers once, and the leader tree's `ru_maxrss` as a floor | `TestMemorySharedByForkedWorkersIsCountedOnce` (both ways: 197 MiB actual, 200 MiB reported; a sum of RSS said 788 MiB) |
| **An out-of-memory kill is named exactly** | The cgroup's `oom_kill` count. Without a cgroup, a SIGKILL is reported as what it may be, and said to be unconfirmed | `TestAnOutOfMemoryKillIsReportedExactly` (a 100 MiB limit) |
| **Limits hold** | `memory` (`MemoryMax`, with `MemorySwapMax=0`) and `cpus` (`CPUQuota`) on the run's scope; a time limit; in a box, the container's limits | `TestATimeLimitStopsTheRun`, and the OOM test |
| **One signal is one signal** | An interrupt goes to the run's leader alone. uv forwards SIGINT and SIGTERM to its child, so signalling the whole group delivered each twice (review H2) | `TestThroughUvAnInterruptReachesTheFileOnce` (real uv) |
| **Nothing is lost silently, and `ok` means ok** | `run.end` carries the telemetry's counts (`telemetry`: `complete`, `producers`, `dropped`, `truncated`, `limited`, `slots_full`, `unclean_exits`, `corrupt_bytes`, `protocol_errors`), and a note when it is incomplete | `TestNoValueARunPassesCanBreakItsRecord`, `TestAHugeEventIsCutToFitAndTheStreamGoesOn` |
| **Events reach the log within a quarter of a second** | Rings are read every 25 ms, and every 50 ms while they have nothing new (a quiet run costs fewer wakeups). The log is flushed every 100 ms, and at once after `run.start`, `run.end`, `stage`, `error`, `artifact`, `hello` and `cli.protocol` | `TestEventsReachTheLogWithinAQuarterSecond` (p99) |
| **A slow terminal never slows the run** | The renderer runs in its own goroutine and queues; the live view redraws at most ten times a second; a terminal that cannot keep up loses lines from the display only (they are all in `output.log`), and is told how many | `TestATerminalThatStopsReadingNeverHoldsTheRun` |

## The cgroup

Where systemd's user manager is reachable, each run gets a transient scope,
`gongoozler-ID.scope`, made with `systemd-run --user --scope`:
- `OOMPolicy=continue`: by default systemd stops a unit when the kernel kills one of its
  processes for memory, which would take the scope, and its numbers, with it;
- the anchor is a small shell (`switchScript` in `launch.go`) that holds the scope open until the
  supervisor has read the run's final numbers, and is the dead man's switch above;
- the run is started inside the scope with `CLONE_INTO_CGROUP` (`SysProcAttr.UseCgroupFD`). If
  the kernel will not (an older kernel), the run starts without one.

**Where the CLI starts matters.** The kernel lets a process start a child in another cgroup only
if it may write the two cgroups' common ancestor. A CLI started in the user's systemd tree (a
desktop's terminal) may. One started in a login session's scope or in a system service (a shell
over SSH or in WSL, a CI runner) may not, though it can still make scopes. So such a CLI first
places itself: it executes itself again through `systemd-run --user --scope`, which moves it into
the user's tree, keeping its PID, terminal, output and signals. Its runs then get cgroups
(`TestARunGetsACgroupWhereverTheCLIIsStarted`). Whether this process can give a run a scope is
probed once, the way a run's scope is made: a scope, and a child started in it. A program that
runs the supervisor as a library (`supervise.Run`) gets cgroups only when it is in the user's tree
itself.

In a container, the container is the cgroup: the supervisor reads its own cgroup's files, which
the container's limits govern. With neither (macOS, a WSL distribution without systemd, or
`--cgroup off`), memory is summed PSS. `--cgroup require` refuses to run without a cgroup.

## How a run ends

`run.end` and `meta.json` say how, in `status`:

| Status | When | Exit code |
| --- | --- | --- |
| `ok` | It exited 0 | 0 |
| `failed` | It exited non-zero, or could not start | its own; 127 if it could not start |
| `oom` | Its cgroup counted an out-of-memory kill | 137, or its own |
| `killed` | A signal ended it | 128 + the signal |
| `interrupted` | SIGINT, SIGTERM or SIGHUP reached the supervisor | 128 + the signal |
| `cancelled` | `gongoozler kill` (SIGUSR1 to the supervisor), a cancelled context, or a server's cancellation | 128 + the signal |
| `timeout` | Its time limit passed | 128 + the signal |
| `lost` | Its supervisor ended before it did (found later, by `ls`, `run`, `attach` or `kill`) | 1 |

A second interrupt, or `gongoozler kill --force`, ends every process of the run at once; a run
still there five grace periods after a SIGTERM is killed. `run.end` also names the stages the run
was in when it did not end `ok` (`open_stages`, innermost last), its memory peak and where it was
measured (`memory_source`), and how long it took to start (`startup`: until its first event).

## Lost runs

A run whose supervisor has gone (the same process, by PID and start time, on this host) reads as
`lost`. The CLI settles such runs before it lists or starts runs (`Settle`): it kills what is
left of the run's process group (by PID and start time, never a process that only reuses the
PID), removes its rings, and writes its meta as `lost`, with a note.

## On disk

```
STATE/runs/ID/meta.json      written when the run starts (status "running"), again once it has a
                             PID, and when it ends
STATE/runs/ID/events.jsonl   every event, numbered `n` in the order the supervisor stored it
STATE/runs/ID/output.log     everything the run printed (the first 10,000 lines are events too)
STATE/runs/ID/artifacts/     the files the run declared, copied and hashed (spec/results.md)
STATE/runs/ID/source/        the file and its lock, as run
```

`STATE` is `$GONGOOZLER_STATE`, else `$XDG_STATE_HOME/gongoozler`, else
`~/.local/state/gongoozler`. Everything under it is its owner's alone: directories 0700, files
0600.

## What the run is given

| Variable | Meaning |
| --- | --- |
| `GONGOOZLER_RING` | Where the run's processes make their rings |
| `GONGOOZLER_RUN_ID` | The run's ID |
| `GONGOOZLER_ARTIFACTS` | Where it writes the files it keeps |
| `GONGOOZLER_PARAMS` | Its parameters, as a JSON object (spec/run-file.md) |
| `PYTHONUNBUFFERED=1` | Unless set already: its output is shown as it is printed. The library, once imported, makes stdout and stderr line-buffered, so a printed line costs one write |
