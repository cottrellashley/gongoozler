package main

import (
	"fmt"
	"os"
	"os/exec"
	"syscall"

	"github.com/cottrellashley/gongoozler/internal/procmem"
)

// placeEnv marks a CLI that has placed itself, so it never does twice.
const placeEnv = "GONGOOZLER_PLACED"

// placeSelf moves the CLI into a systemd user scope of its own when that is what it takes to give
// its runs cgroups (spec/supervisor.md, "The cgroup"). A CLI started in a login session's scope or
// a system service (over SSH, in WSL, on a CI runner) can make scopes but not start runs in them:
// the kernel will not let it start a child in a cgroup outside its own subtree. So it executes
// itself again through `systemd-run --user --scope`, which moves it into the user's systemd tree.
// Its PID, terminal, output and signals stay the same. Commands that run files here call it once
// their flags are read; it returns when there is nothing to do, or nothing it can do.
func placeSelf(cgroup string) {
	if cgroup == "off" || os.Getenv(placeEnv) != "" || procmem.ScopesAvailable() || !procmem.ScopesCanBeMade() {
		return
	}
	self, err := os.Executable()
	if err != nil {
		return
	}
	systemdRun, err := exec.LookPath("systemd-run")
	if err != nil {
		return
	}
	unit := fmt.Sprintf("gongoozler-cli-%d", os.Getpid())
	args := append([]string{"systemd-run", "--user", "--scope", "--quiet", "--collect", "--unit", unit, "--", self}, os.Args[1:]...)
	// Exec returns only if it failed; the CLI then goes on where it is, and runs without cgroups.
	_ = syscall.Exec(systemdRun, args, append(os.Environ(), placeEnv+"=1"))
}
