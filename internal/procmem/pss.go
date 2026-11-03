package procmem

import (
	"bufio"
	"bytes"
	"os"
	"path/filepath"
	"strconv"
	"strings"
)

// PSS is a process's proportional set size, in bytes, from /proc/PID/smaps_rollup: its private
// pages, plus each shared page divided by the number of processes sharing it. Summed over a tree
// of forked workers it counts shared memory once, where a sum of RSS counts it once per worker.
func PSS(proc string, pid int) (int64, error) {
	f, err := os.Open(filepath.Join(proc, strconv.Itoa(pid), "smaps_rollup"))
	if err != nil {
		return 0, err
	}
	defer f.Close()
	s := bufio.NewScanner(f)
	for s.Scan() {
		if rest, ok := strings.CutPrefix(s.Text(), "Pss:"); ok {
			kb, err := strconv.ParseInt(strings.TrimSpace(strings.TrimSuffix(strings.TrimSpace(rest), "kB")), 10, 64)
			return kb * 1024, err
		}
	}
	return 0, s.Err()
}

// Members lists a run's processes without a cgroup: everything in process group pgid, and every
// child of parent (the supervisor, a subreaper) except skip. Orphans the run leaves are reparented
// to the supervisor, so they are found even when they left the group.
func Members(proc string, pgid, parent, skip int) []int {
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
		st, err := Stat(proc, pid)
		if err != nil {
			continue
		}
		if st.PGID == pgid || (st.PPID == parent && pid != skip) {
			out = append(out, pid)
		}
	}
	return out
}

// GroupPSS sums PSS over a run's processes (Members) and counts them.
func GroupPSS(proc string, pgid, parent, skip int) (int64, int) {
	var total int64
	n := 0
	for _, pid := range Members(proc, pgid, parent, skip) {
		if v, err := PSS(proc, pid); err == nil {
			total += v
			n++
		}
	}
	return total, n
}

// ProcStat is what the supervisor needs from /proc/PID/stat.
type ProcStat struct {
	State     byte
	PPID      int
	PGID      int
	StartTime uint64 // clock ticks after boot: with the PID, it names one process for good
	Comm      string
}

// Stat reads /proc/PID/stat. The command name may hold spaces and parentheses, so the fields
// are read after the last ')'.
func Stat(proc string, pid int) (ProcStat, error) {
	b, err := os.ReadFile(filepath.Join(proc, strconv.Itoa(pid), "stat"))
	if err != nil {
		return ProcStat{}, err
	}
	open := bytes.IndexByte(b, '(')
	i := bytes.LastIndexByte(b, ')')
	if open < 0 || i < 0 || i < open {
		return ProcStat{}, os.ErrInvalid
	}
	f := bytes.Fields(b[i+1:])
	if len(f) < 20 {
		return ProcStat{}, os.ErrInvalid
	}
	ppid, _ := strconv.Atoi(string(f[1]))
	pgid, _ := strconv.Atoi(string(f[2]))
	start, _ := strconv.ParseUint(string(f[19]), 10, 64)
	return ProcStat{State: f[0][0], PPID: ppid, PGID: pgid, StartTime: start, Comm: string(b[open+1 : i])}, nil
}

// Alive reports whether pid is running and is the process that started at start (not a later
// process that reused its PID).
func Alive(proc string, pid int, start uint64) bool {
	st, err := Stat(proc, pid)
	return err == nil && st.State != 'Z' && st.StartTime == start
}
