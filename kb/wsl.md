# Running locally under WSL

Local users run gongoozler in WSL2: a Linux distribution on Windows, with a real Linux kernel. So
the CLI's Linux code runs unchanged. What matters:

- **Memory is the WSL virtual machine's.** `/proc/meminfo`'s `MemAvailable`, which the CLI
  reports, is what a run can use inside WSL, and the VM's size is set in Windows (`.wslconfig`).
  A run needing more than the VM holds is a run for a box.
- **`/proc` is there:** without a cgroup, the supervisor measures a run's processes by PSS from
  it, as on any Linux.
- **A cgroup needs systemd.** A run gets a cgroup of its own (exact memory, limits, the dead man's
  switch) only where systemd's user manager runs: WSL distributions can enable systemd
  (`[boot] systemd=true` in `/etc/wsl.conf`). A WSL shell sits in a login session's scope, so the
  CLI places itself in the user's tree first (spec/supervisor.md, "The cgroup"). Without systemd
  the supervisor falls back to PSS and the parent-death signal, and says so
  (`memory_source: "pss"`).
- **Rings live in memory.** A run's rings go in `/dev/shm`, a tmpfs in WSL2 as on Linux, or else
  the run's directory under `~/.local/state`: never on a Windows drive (`/mnt/c/...`), where shared
  mappings are unreliable and slow.
- **Keep run files in the Linux filesystem too** (`~/...`), not `/mnt/c`. Reads across the
  Windows boundary are slow, and uv's cache and environments do many of them.
- **The browser is on Windows.** A page the CLI serves on `localhost` inside WSL is reached from
  Windows at the same address, while WSL's localhost forwarding is on (the default). That matters
  for `gongoozler ui`, a later slice.
- **Boxes are Docker containers,** so a WSL user with Docker (Docker Desktop's WSL integration, or
  Docker installed in the distribution) can run the server and workers locally too
  (spec/server.md).

To check, not yet tested in WSL itself: the CLI has been exercised on Linux (Omarchy, kernel 7.2),
not inside a WSL distribution.
