// Package procmem measures a run's memory from /proc: Linux, and WSL2, which runs a Linux kernel.
//
// A run is a process group: the CLI starts the run's first process in a group of its own, and
// everything it starts (uv, then Python, then any workers) stays in that group unless it leaves.
// Membership by group also catches processes whose parent has exited.
package procmem

import (
	"bufio"
	"bytes"
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"sync"
	"time"
)

var pageSize = int64(os.Getpagesize())

// GroupRSS is the summed resident memory, in bytes, of every live process in group pgid, and how
// many processes there are.
func GroupRSS(proc string, pgid int) (rss int64, procs int, err error) {
	entries, err := os.ReadDir(proc)
	if err != nil {
		return 0, 0, err
	}
	for _, d := range entries {
		pid, err := strconv.Atoi(d.Name())
		if err != nil {
			continue
		}
		g, err := group(proc, pid)
		if err != nil || g != pgid {
			continue // gone, or not ours
		}
		r, err := resident(proc, pid)
		if err != nil {
			continue
		}
		rss += r
		procs++
	}
	return rss, procs, nil
}

// group reads a process's group from /proc/PID/stat. The command name may hold spaces and
// parentheses, so the fields are read after the last ')'.
func group(proc string, pid int) (int, error) {
	b, err := os.ReadFile(filepath.Join(proc, strconv.Itoa(pid), "stat"))
	if err != nil {
		return 0, err
	}
	i := bytes.LastIndexByte(b, ')')
	if i < 0 {
		return 0, errors.New("malformed stat")
	}
	f := bytes.Fields(b[i+1:]) // state, ppid, pgrp, ...
	if len(f) < 3 {
		return 0, errors.New("malformed stat")
	}
	return strconv.Atoi(string(f[2]))
}

// resident reads a process's resident set from /proc/PID/statm (pages).
func resident(proc string, pid int) (int64, error) {
	b, err := os.ReadFile(filepath.Join(proc, strconv.Itoa(pid), "statm"))
	if err != nil {
		return 0, err
	}
	f := bytes.Fields(b)
	if len(f) < 2 {
		return 0, errors.New("malformed statm")
	}
	pages, err := strconv.ParseInt(string(f[1]), 10, 64)
	return pages * pageSize, err
}

// Available is the kernel's estimate of memory available for new work (MemAvailable), in bytes.
// In WSL2 it is the WSL virtual machine's, which is what a run there can use.
func Available(proc string) (int64, error) {
	f, err := os.Open(filepath.Join(proc, "meminfo"))
	if err != nil {
		return 0, err
	}
	defer f.Close()
	s := bufio.NewScanner(f)
	for s.Scan() {
		var kb int64
		if n, _ := fmt.Sscanf(s.Text(), "MemAvailable: %d kB", &kb); n == 1 {
			return kb * 1024, nil
		}
	}
	return 0, errors.New("no MemAvailable in meminfo")
}

// Sample is one reading of a run's memory.
type Sample struct {
	RSS, Peak, Available int64
	Procs                int
}

// Watcher samples a process group's memory until stopped, keeping the peak.
type Watcher struct {
	mu   sync.Mutex
	last Sample
	done chan struct{}
}

// Watch samples group pgid every `every`, calling fn (if not nil) with each sample.
func Watch(ctx context.Context, proc string, pgid int, every time.Duration, fn func(Sample)) *Watcher {
	w := &Watcher{done: make(chan struct{})}
	go func() {
		defer close(w.done)
		t := time.NewTicker(every)
		defer t.Stop()
		for {
			w.sample(proc, pgid, fn)
			select {
			case <-ctx.Done():
				return
			case <-t.C:
			}
		}
	}()
	return w
}

func (w *Watcher) sample(proc string, pgid int, fn func(Sample)) {
	rss, procs, err := GroupRSS(proc, pgid)
	if err != nil {
		return
	}
	avail, _ := Available(proc)
	w.mu.Lock()
	s := Sample{RSS: rss, Peak: max(w.last.Peak, rss), Available: avail, Procs: procs}
	w.last = s
	w.mu.Unlock()
	if fn != nil {
		fn(s)
	}
}

// Last is the latest sample, with the peak so far.
func (w *Watcher) Last() Sample { w.mu.Lock(); defer w.mu.Unlock(); return w.last }

// Wait blocks until the watcher has stopped.
func (w *Watcher) Wait() { <-w.done }
