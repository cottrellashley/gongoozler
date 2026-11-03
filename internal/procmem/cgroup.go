package procmem

import (
	"bufio"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"
)

// Cgroup is a cgroup v2 directory that holds a run: the kernel accounts its memory exactly,
// counts its out-of-memory kills, and can kill everything in it at once.
type Cgroup struct {
	Path string
	// Own is true when the run alone is in it (a systemd scope); false when the supervisor shares
	// it (a container whose cgroup is the box). Kill is refused when it is shared.
	Own bool
}

func (c Cgroup) read(name string) (string, error) {
	b, err := os.ReadFile(filepath.Join(c.Path, name))
	return strings.TrimSpace(string(b)), err
}

// Current is memory.current: every byte charged to the cgroup now, page cache included.
func (c Cgroup) Current() (int64, error) {
	s, err := c.read("memory.current")
	if err != nil {
		return 0, err
	}
	return strconv.ParseInt(s, 10, 64)
}

// Peak is memory.peak (Linux 5.19 and later): the most ever charged at once.
func (c Cgroup) Peak() (int64, error) {
	s, err := c.read("memory.peak")
	if err != nil {
		return 0, err
	}
	return strconv.ParseInt(s, 10, 64)
}

// OOMKills is memory.events' oom_kill: processes the kernel killed for memory.
func (c Cgroup) OOMKills() (int64, error) {
	f, err := os.Open(filepath.Join(c.Path, "memory.events"))
	if err != nil {
		return 0, err
	}
	defer f.Close()
	s := bufio.NewScanner(f)
	for s.Scan() {
		if k, v, ok := strings.Cut(s.Text(), " "); ok && k == "oom_kill" {
			return strconv.ParseInt(v, 10, 64)
		}
	}
	return 0, s.Err()
}

// Procs lists the processes in the cgroup.
func (c Cgroup) Procs() ([]int, error) {
	s, err := c.read("cgroup.procs")
	if err != nil {
		return nil, err
	}
	var out []int
	for _, f := range strings.Fields(s) {
		if p, err := strconv.Atoi(f); err == nil {
			out = append(out, p)
		}
	}
	return out, nil
}

// Kill kills everything in an owned cgroup: cgroup.kill (Linux 5.14 and later), or SIGKILL to
// each process.
func (c Cgroup) Kill() error {
	if !c.Own {
		return errors.New("the cgroup is shared with the supervisor")
	}
	if err := os.WriteFile(filepath.Join(c.Path, "cgroup.kill"), []byte("1"), 0); err == nil {
		return nil
	}
	procs, err := c.Procs()
	for _, p := range procs {
		syscall.Kill(p, syscall.SIGKILL)
	}
	return err
}

// CgroupOf is the cgroup v2 path of a process (from /proc/PID/cgroup).
func CgroupOf(proc string, pid int) (string, error) {
	b, err := os.ReadFile(filepath.Join(proc, strconv.Itoa(pid), "cgroup"))
	if err != nil {
		return "", err
	}
	for _, line := range strings.Split(string(b), "\n") {
		if p, ok := strings.CutPrefix(line, "0::"); ok {
			return filepath.Join("/sys/fs/cgroup", p), nil
		}
	}
	return "", errors.New("no cgroup v2 entry")
}

// WaitForScope waits until pid has moved into a cgroup whose name ends with unit (systemd-run
// moves itself into the scope before it executes the command), and returns it.
func WaitForScope(proc string, pid int, unit string, timeout time.Duration) (Cgroup, error) {
	deadline := time.Now().Add(timeout)
	for {
		p, err := CgroupOf(proc, pid)
		if err == nil && filepath.Base(p) == unit {
			return Cgroup{Path: p, Own: true}, nil
		}
		if time.Now().After(deadline) {
			if err == nil {
				err = fmt.Errorf("still in %s", p)
			}
			return Cgroup{}, fmt.Errorf("process %d never reached scope %s: %w", pid, unit, err)
		}
		time.Sleep(5 * time.Millisecond)
	}
}

// SelfContained reports the cgroup this process is in, when it is a container's own (a cgroup
// namespace whose root is ours, with memory accounting): the box is the cgroup.
func SelfContained(proc string) (Cgroup, bool) {
	b, err := os.ReadFile(filepath.Join(proc, "self", "cgroup"))
	if err != nil || strings.TrimSpace(string(b)) != "0::/" {
		return Cgroup{}, false
	}
	c := Cgroup{Path: "/sys/fs/cgroup"}
	if _, err := c.Current(); err != nil {
		return Cgroup{}, false
	}
	return c, true
}

var (
	scopeOnce sync.Once
	scopeOK   bool
)

// ScopesAvailable reports whether `systemd-run --user --scope` works here, trying it once.
func ScopesAvailable() bool {
	scopeOnce.Do(func() {
		path, err := exec.LookPath("systemd-run")
		if err != nil {
			return
		}
		cmd := exec.Command(path, "--user", "--scope", "--quiet", "--collect", "--", "true")
		scopeOK = cmd.Run() == nil
	})
	return scopeOK
}
