# What we rely on from Linux and systemd

"Read" means taken from documentation; "observed" means seen on this machine (Linux 7.2,
systemd's user manager, cgroup v2), in this repo's tests. The supervisor uses each fact in
`internal/supervise` and `internal/procmem` (spec/supervisor.md).

| Fact | Source | Used by |
| --- | --- | --- |
| `systemd-run --user --scope --unit NAME -- CMD` makes a transient scope in the user's manager and execs `CMD` in it; the scope's cgroup is `…/user@UID.service/app.slice/NAME.scope`, and its files (`memory.current`, `memory.peak`, `memory.events`, `cgroup.procs`, `cgroup.kill`) are the user's | Observed | The run's cgroup |
| **`OOMPolicy=continue` is needed.** By default systemd stops a unit when the kernel kills one of its processes for memory, which removed the scope and its numbers before they were read: a run killed for memory read as merely `killed` | Observed: `TestAnOutOfMemoryKillIsReportedExactly` failed without it | Exact `oom` status |
| `MemoryMax` with `MemorySwapMax=0` is a hard memory limit; `memory.events`' `oom_kill` counts the kernel's kills in the cgroup | Read: kernel `cgroup-v2.rst`. Observed | Limits, and naming an OOM |
| `CLONE_INTO_CGROUP` (Linux 5.7, Go's `SysProcAttr.UseCgroupFD`) starts a child directly in a cgroup, given an open descriptor of its directory, so not one instruction of the run runs outside it | Read: `clone3(2)`. Observed | Starting the run in its scope |
| **Delegation containment:** a process may move a process into a cgroup, or start a child in one, only if it may write `cgroup.procs` of the common ancestor of the source and the destination. From a login session's scope (`user-UID.slice/session-N.scope`) or a system service, the common ancestor with the user manager's tree is root's, so `CLONE_INTO_CGROUP` into a user scope is refused | Read: kernel `cgroup-v2.rst`, "Delegation Containment". Observed on GitHub's Ubuntu 24.04 runner: scopes were made, and every run fell back to PSS | The probe in `procmem.ScopesAvailable` |
| `systemd-run --user --scope -- CMD` works even from outside the user's tree: the caller is moved into the new scope (systemd does it), then it executes `CMD` as the same process, with its PID, terminal and signals | Observed on the same runner (`systemd-run --user --scope -- true`) | The CLI placing itself (`cmd/gongoozler/place.go`) |
| `cgroup.kill` (Linux 5.14): writing `1` kills every process in the cgroup with SIGKILL, at once | Read: `cgroup-v2.rst`. Observed: writable by the user in a user scope | The dead man's switch |
| **A parent-death signal follows the thread, not the process.** `PR_SET_PDEATHSIG` fires when the *thread* that created the child exits. Go starts children from whichever OS thread it likes, which may end at any time, so the starting goroutine is locked to its thread (`runtime.LockOSThread`) until the run ends | Read: `prctl(2)`. Observed | The run dying with its supervisor |
| The parent-death signal survives `execve` (unless set-user-ID), so `systemd-run`'s, exec'd into the anchor's shell, still fires | Read: `prctl(2)`. Observed: `TestARunDiesWithinASecondOfItsSupervisor` | The anchor |
| `PR_SET_CHILD_SUBREAPER` (36) makes a process the reaper of its descendants' orphans, in place of init | Read: `prctl(2)` | Finding and stopping leftovers |
| A process's PSS (`/proc/PID/smaps_rollup`) shares each page among the processes mapping it, so summing PSS counts copy-on-write memory once; summing RSS counted it once per process (4× for four forked workers) | Read: `proc(5)`. Observed: `TestMemorySharedByForkedWorkersIsCountedOnce` | Memory without a cgroup |
| `/proc/PID/stat`'s fields after the last `)`: the command name may hold spaces and parentheses. Field 22, the start time, with the PID names one process for good | Read: `proc(5)`. Observed: `TestACommandNameWithParenthesesStillParses` | Telling a live supervisor from a reused PID |
| A process's own cgroup is `/proc/self/cgroup`'s `0::PATH` line; in a container it is the container's | Read | The anchor's switch; the in-container mode |
| Linux will not hand out a PID still in use as a process group ID | Read: `credentials(7)` | Settling a lost run's survivors |
