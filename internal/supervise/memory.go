package supervise

import (
	"context"
	"io"
	"os"
	"sync"
	"sync/atomic"
	"time"

	"bytes"

	"github.com/cottrellashley/gongoozler/internal/procmem"
)

// memory watches the run's memory where the kernel accounts it: its cgroup (memory.current,
// every half second), or, with no cgroup, the summed PSS of its processes (every second), which
// counts memory shared by forked workers once. Never a sum of RSS. It reports a resource event
// every second.
type memory struct {
	source string
	stopc  chan struct{}
	done   chan struct{}
	mu     sync.Mutex
	last   procmem.Sample
	// The cgroup's own peak and out-of-memory count, as last read: what is known if the cgroup is
	// gone by the time the run ends.
	cgPeak, oomKills int64
}

func watchMemory(ctx context.Context, o Options, p *process, pl *pipeline) *memory {
	m := &memory{source: "pss", stopc: make(chan struct{}), done: make(chan struct{})}
	every := time.Second
	if p.cgroup != nil {
		m.source, every = "cgroup", 500*time.Millisecond
	}
	limit := int64(0)
	if p.cgroup != nil && p.cgroup.Own {
		limit = o.Memory
	}
	go func() {
		defer close(m.done)
		t := time.NewTicker(every)
		defer t.Stop()
		var lastRes time.Time
		for {
			var cur int64
			var procs int
			var cgPeak, ooms int64
			if p.cgroup != nil {
				cur, _ = p.cgroup.Current()
				cgPeak, _ = p.cgroup.Peak()
				ooms, _ = p.cgroup.OOMKills()
				procs = len(p.members())
			} else {
				cur, procs = procmem.GroupPSS(o.Proc, p.pid, groupParent(), p.anchorPID())
			}
			avail, _ := procmem.Available(o.Proc)
			if limit > 0 && limit-cur < avail {
				avail = max(limit-cur, 0)
			}
			m.mu.Lock()
			m.last = procmem.Sample{RSS: cur, Peak: max(m.last.Peak, cur), Available: avail, Procs: procs}
			m.cgPeak, m.oomKills = max(m.cgPeak, cgPeak), max(m.oomKills, ooms)
			s := m.last
			m.mu.Unlock()
			if time.Since(lastRes) >= time.Second {
				lastRes = time.Now()
				pl.cli("resource", map[string]any{"memory": s.RSS, "peak": s.Peak, "available": s.Available, "procs": s.Procs, "source": m.source})
			}
			select {
			case <-ctx.Done():
			case <-m.stopc:
				return
			case <-t.C:
			}
		}
	}()
	return m
}

func groupParent() int {
	if runsActive() > 1 {
		return -1
	}
	return os.Getpid()
}

// stop ends the watch and returns the last sample with the peak seen, and the cgroup's peak and
// out-of-memory count as last read.
func (m *memory) stop() (procmem.Sample, int64, int64) {
	close(m.stopc)
	<-m.done
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.last, m.cgPeak, m.oomKills
}

// output stores what the run prints: every line in output.log, the first OutputCap as events,
// and each one to the renderer.
type output struct {
	f        io.Writer
	mu       sync.Mutex
	writeErr error // the first write to output.log that failed
	pl       *pipeline
	o        Options
	n        atomic.Int64
}

func newOutput(f io.Writer, pl *pipeline, o Options) *output { return &output{f: f, pl: pl, o: o} }

func (out *output) writer(stream string) io.Writer { return &lines{stream: stream, fn: out.line} }

func (out *output) line(stream, text string) {
	out.mu.Lock()
	if _, err := out.f.Write([]byte(stream + " " + text + "\n")); err != nil && out.writeErr == nil {
		out.writeErr = err
	}
	out.mu.Unlock()
	if out.o.Render != nil {
		out.o.Render.Output(stream, text)
	}
	if out.n.Add(1) <= int64(out.o.OutputCap) {
		out.pl.cli("output", map[string]any{"stream": stream, "line": text})
	}
}

func (out *output) stored() int64 { return out.n.Load() }

// failed is the first write to output.log that failed, if one did.
func (out *output) failed() error { out.mu.Lock(); defer out.mu.Unlock(); return out.writeErr }

// lines is an io.Writer that calls fn once per complete line.
type lines struct {
	mu     sync.Mutex
	stream string
	buf    []byte
	fn     func(stream, line string)
}

func (l *lines) Write(p []byte) (int, error) {
	l.mu.Lock()
	defer l.mu.Unlock()
	l.buf = append(l.buf, p...)
	start := 0
	for {
		i := bytes.IndexByte(l.buf[start:], '\n')
		if i < 0 {
			break
		}
		l.fn(l.stream, string(bytes.TrimSuffix(l.buf[start:start+i], []byte("\r"))))
		start += i + 1
	}
	l.buf = append(l.buf[:0], l.buf[start:]...)
	if len(l.buf) > 1<<20 { // a megabyte with no newline: pass it on as it is
		l.fn(l.stream, string(l.buf))
		l.buf = l.buf[:0]
	}
	return len(p), nil
}
