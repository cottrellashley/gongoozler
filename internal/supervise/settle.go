package supervise

import (
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"syscall"
	"time"

	"github.com/cottrellashley/gongoozler/internal/procmem"
	"github.com/cottrellashley/gongoozler/internal/runs"
)

// Settle records the runs under state whose supervisor died as lost (spec/supervisor.md, "Lost
// runs"): it stops what such a run left running, removes its rings, and writes its meta with
// status "lost". The CLI settles before it lists or starts runs. It returns the runs it settled.
func Settle(state, proc string) []runs.Meta {
	all, err := runs.List(state)
	if err != nil {
		return nil
	}
	var settled []runs.Meta
	for _, m := range all {
		if m.Status != "lost" {
			continue
		}
		stored, err := runs.ReadMeta(state, m.ID)
		if err != nil || stored.Status != "running" {
			continue
		}
		var notes []string
		if n := stopSurvivors(proc, stored); n > 0 {
			notes = append(notes, fmt.Sprintf("stopped %d processes it left", n))
		}
		removeRings(state, stored.ID)
		stored.Status, stored.Ended = "lost", time.Now()
		if stored.ExitCode == 0 {
			stored.ExitCode = 1
		}
		note := "its supervisor ended before the run did"
		if len(notes) > 0 {
			note += "; " + notes[0]
		}
		stored.Note = note
		if runs.WriteMeta(state, stored) == nil {
			settled = append(settled, stored)
		}
	}
	return settled
}

// stopSurvivors kills what is left of a lost run's process group: the whole group while its leader
// lives (the same process, by PID and start time); once the leader is gone, the members still in its
// group, unless its PID now names another process (whose group it would be).
func stopSurvivors(proc string, m runs.Meta) int {
	if m.RunPID <= 0 {
		return 0
	}
	if !procmem.Alive(proc, m.RunPID, m.RunStart) {
		if _, err := procmem.Stat(proc, m.RunPID); err == nil {
			return 0
		}
	}
	survivors := groupMembers(proc, m.RunPID, m.RunStart)
	for _, pid := range survivors {
		syscall.Kill(pid, syscall.SIGKILL)
	}
	return len(survivors)
}

func groupMembers(proc string, pgid int, since uint64) []int {
	entries, err := os.ReadDir(proc)
	if err != nil {
		return nil
	}
	var out []int
	for _, d := range entries {
		pid, err := strconv.Atoi(d.Name())
		if err != nil {
			continue
		}
		if st, err := procmem.Stat(proc, pid); err == nil && st.PGID == pgid && st.StartTime >= since && st.State != 'Z' {
			out = append(out, pid)
		}
	}
	return out
}

// removeRings removes a run's ring directory, in shared memory or in its run directory.
func removeRings(state, id string) {
	os.RemoveAll(filepath.Join(runs.Dir(state, id), "ring"))
	os.RemoveAll(filepath.Join("/dev/shm", fmt.Sprintf("gongoozler-%d", os.Getuid()), id))
}
