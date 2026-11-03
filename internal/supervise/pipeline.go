package supervise

import (
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/cottrellashley/gongoozler/internal/events"
)

// pipeline stores every event of a run in the order it arrives. One goroutine numbers and stores;
// it flushes the log on a ticker and at once after urgent events, hands each event to OnEvent and
// the renderer (which queues and never makes it wait), and passes artifact declarations to a worker
// that copies and hashes them, outside the run.
type pipeline struct {
	in     chan events.Event
	log    *events.Log
	o      Options
	artDir string
	done   chan struct{}
	hello  atomic.Int64

	declMu   sync.Mutex
	declCond *sync.Cond
	decls    []events.Event
	declEnd  bool
	artDone  chan struct{}
}

const barrierKind = "\x00barrier"

func newPipeline(log *events.Log, o Options, artDir string) *pipeline {
	pl := &pipeline{in: make(chan events.Event, 8192), log: log, o: o, artDir: artDir, done: make(chan struct{}), artDone: make(chan struct{})}
	pl.declCond = sync.NewCond(&pl.declMu)
	go pl.store()
	go pl.artifacts()
	return pl
}

// cli queues an event the supervisor makes itself.
func (pl *pipeline) cli(kind string, fields map[string]any) { pl.in <- events.New(kind, fields) }

// producer queues an event from one of the run's processes, stamped with when it was received.
func (pl *pipeline) producer(e events.Event) {
	e["rt"] = float64(time.Now().UnixNano()) / 1e9
	pl.in <- e
}

func (pl *pipeline) store() {
	defer close(pl.done)
	tick := time.NewTicker(100 * time.Millisecond)
	defer tick.Stop()
	for {
		select {
		case e, ok := <-pl.in:
			if !ok {
				_ = pl.log.Flush() // a log that cannot be written fails close, which reports it
				return
			}
			if e.Kind() == barrierKind {
				e["\x00ack"].(chan struct{}) <- struct{}{}
				continue
			}
			if e.Kind() == "hello" {
				pl.hello.CompareAndSwap(0, time.Now().UnixNano())
			}
			e, err := pl.log.Append(e)
			if err != nil {
				fmt.Fprintf(os.Stderr, "gongoozler: cannot store an event: %v\n", err)
			}
			if events.Urgent(e.Kind()) {
				_ = pl.log.Flush()
			}
			if pl.o.OnEvent != nil {
				pl.o.OnEvent(e)
			}
			if pl.o.Render != nil {
				pl.o.Render.Event(e)
			}
			if e.Kind() == "artifact.declare" {
				pl.declMu.Lock()
				pl.decls = append(pl.decls, e)
				pl.declMu.Unlock()
				pl.declCond.Signal()
			}
		case <-tick.C:
			_ = pl.log.Flush()
		}
	}
}

func (pl *pipeline) artifacts() {
	defer close(pl.artDone)
	for {
		pl.declMu.Lock()
		for len(pl.decls) == 0 && !pl.declEnd {
			pl.declCond.Wait()
		}
		if len(pl.decls) == 0 {
			pl.declMu.Unlock()
			return
		}
		d := pl.decls[0]
		pl.decls = pl.decls[1:]
		pl.declMu.Unlock()
		pl.in <- storeArtifact(d, pl.artDir)
	}
}

// waitArtifacts waits until every artifact declared so far is stored: a barrier makes sure the
// store goroutine has seen every queued declaration first.
func (pl *pipeline) waitArtifacts() {
	ack := make(chan struct{})
	pl.in <- events.Event{"kind": barrierKind, "\x00ack": ack}
	<-ack
	pl.declMu.Lock()
	pl.declEnd = true
	pl.declMu.Unlock()
	pl.declCond.Broadcast()
	<-pl.artDone
}

func (pl *pipeline) firstHello() time.Time {
	if n := pl.hello.Load(); n != 0 {
		return time.Unix(0, n)
	}
	return time.Time{}
}

func (pl *pipeline) process(int) {}

// close stores what is queued, then flushes, syncs and closes the log.
func (pl *pipeline) close() error {
	close(pl.in)
	<-pl.done
	return pl.log.Close()
}

func (pl *pipeline) count() int64 { return pl.log.Count() }

// storeArtifact copies a declared file into the run's artifacts (unless it is already there) and
// hashes it, returning the `artifact` event that records it.
func storeArtifact(d events.Event, artDir string) events.Event {
	src := d.Str("path")
	name := filepath.Base(strings.TrimSpace(d.Str("name")))
	if name == "" || name == "." || name == ".." || name == string(filepath.Separator) {
		name = filepath.Base(src)
	}
	dst := filepath.Join(artDir, name)
	fields := map[string]any{"name": name, "source": src}
	n, sum, err := hashInto(src, dst)
	if err != nil {
		fields["error"] = err.Error()
	} else {
		fields["path"], fields["bytes"], fields["sha256"] = dst, n, sum
	}
	return events.New("artifact", fields)
}

func hashInto(src, dst string) (int64, string, error) {
	in, err := os.Open(src)
	if err != nil {
		return 0, "", err
	}
	defer in.Close()
	h := sha256.New()
	si, err := in.Stat()
	if err != nil {
		return 0, "", err
	}
	if di, err := os.Stat(dst); err == nil && os.SameFile(si, di) {
		n, err := io.Copy(h, in)
		return n, hex.EncodeToString(h.Sum(nil)), err
	}
	tmp := dst + ".partial"
	out, err := os.OpenFile(tmp, os.O_CREATE|os.O_WRONLY|os.O_TRUNC, 0o600)
	if err != nil {
		return 0, "", err
	}
	n, err := io.Copy(io.MultiWriter(out, h), in)
	if cerr := out.Close(); err == nil {
		err = cerr
	}
	if err == nil {
		err = os.Rename(tmp, dst)
	}
	if err != nil {
		os.Remove(tmp)
		return 0, "", err
	}
	return n, hex.EncodeToString(h.Sum(nil)), nil
}
