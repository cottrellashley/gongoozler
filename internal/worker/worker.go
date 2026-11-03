// Package worker runs a server's jobs (spec/server.md, "Workers"). A worker leases jobs that fit
// what it has free, runs each with a launcher (the same supervisor as a local run, here or in a
// container of its own), streams the run's events to the server as they are stored, uploads its
// artifacts, and reports its end. It renews each lease while the run lasts; a run whose lease is
// lost is stopped, and one the server cancels is cancelled.
package worker

import (
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"runtime"
	"sync"
	"sync/atomic"
	"time"

	"github.com/cottrellashley/gongoozler/internal/api"
	"github.com/cottrellashley/gongoozler/internal/client"
	"github.com/cottrellashley/gongoozler/internal/events"
	"github.com/cottrellashley/gongoozler/internal/procmem"
	"github.com/cottrellashley/gongoozler/internal/runs"
)

// Launcher runs one job's file, already fetched into dir, to its end. It passes every event the
// run's supervisor stores to emit, in order, and stops the run when ctx is done. It returns the
// run's record and the paths of the artifacts it kept.
type Launcher interface {
	Run(ctx context.Context, job api.Job, dir string, emit func(events.Event)) (runs.Meta, []string, error)
}

// Config describes a worker.
type Config struct {
	Client *client.Client
	Name   string
	// Slots is how many runs it runs at once. Default 1.
	Slots int
	// Memory and CPUs are what it may give its runs; a job that declares more waits for another
	// worker. Default: the machine's available memory and its CPUs.
	Memory int64
	CPUs   float64
	// Work holds each job's files while it runs.
	Work     string
	Launcher Launcher
	// Heartbeat is how often a lease is renewed. Default: a third of the lease's TTL.
	Heartbeat time.Duration
	// Wait is how long a request for a job waits for one. Default 20 s.
	Wait time.Duration
	Log  io.Writer
}

var (
	errLeaseLost = errors.New("the server gave the job to another worker")
	errCancelled = errors.New("cancelled on the server")
	errShutdown  = errors.New("the worker is shutting down")
)

// Run takes jobs until ctx is done. Then it stops its runs and gives their jobs back to the server,
// to run elsewhere, and returns.
func Run(ctx context.Context, cfg Config) error {
	if cfg.Slots <= 0 {
		cfg.Slots = 1
	}
	if cfg.CPUs <= 0 {
		cfg.CPUs = float64(runtime.NumCPU())
	}
	if cfg.Wait <= 0 {
		cfg.Wait = 20 * time.Second
	}
	if cfg.Log == nil {
		cfg.Log = io.Discard
	}
	if cfg.Name == "" {
		host, _ := os.Hostname()
		cfg.Name = fmt.Sprintf("%s-%d", host, os.Getpid())
	}
	w := &worker{cfg: cfg}
	slots := make(chan struct{}, cfg.Slots)
	var wg sync.WaitGroup
	backoff := 250 * time.Millisecond
	for ctx.Err() == nil {
		select {
		case slots <- struct{}{}:
		case <-ctx.Done():
			continue
		}
		job, err := cfg.Client.Lease(ctx, w.offer())
		if err != nil || job == nil {
			<-slots
			if err != nil && ctx.Err() == nil {
				fmt.Fprintf(cfg.Log, "worker %s: cannot reach the server: %v\n", cfg.Name, err)
				select {
				case <-ctx.Done():
				case <-time.After(backoff):
				}
				backoff = min(backoff*2, 10*time.Second)
			}
			continue
		}
		backoff = 250 * time.Millisecond
		w.reserve(job.Spec, 1)
		wg.Add(1)
		go func() {
			defer wg.Done()
			defer func() { <-slots }()
			defer w.reserve(job.Spec, -1)
			w.do(ctx, job)
		}()
	}
	wg.Wait()
	return nil
}

type worker struct {
	cfg  Config
	mu   sync.Mutex
	mem  int64
	cpus float64
}

func (w *worker) offer() api.Offer {
	w.mu.Lock()
	defer w.mu.Unlock()
	mem := w.cfg.Memory
	if mem == 0 {
		mem = available()
	}
	return api.Offer{Worker: w.cfg.Name, Memory: max(mem-w.mem, 0), CPUs: max(w.cfg.CPUs-w.cpus, 0), Wait: w.cfg.Wait.Seconds()}
}

func (w *worker) reserve(s api.Spec, sign int) {
	w.mu.Lock()
	w.mem += int64(sign) * s.Memory
	w.cpus += float64(sign) * s.CPUs
	w.mu.Unlock()
}

func (w *worker) logf(format string, args ...any) {
	fmt.Fprintf(w.cfg.Log, "worker %s: %s\n", w.cfg.Name, fmt.Sprintf(format, args...))
}

// do runs one job: fetch, run with heartbeats, push, upload, complete.
func (w *worker) do(shutdown context.Context, job *api.Job) {
	c := w.cfg.Client
	dir := filepath.Join(w.cfg.Work, "jobs", fmt.Sprintf("%s-%d", job.ID, job.Attempt))
	defer os.RemoveAll(dir)
	ctx, cancel := context.WithCancelCause(context.Background())
	defer cancel(nil)
	if err := fetch(ctx, c, job, dir); err != nil {
		w.logf("run %s: cannot fetch its files: %v", job.ID, err)
		w.fail(job, "its worker could not fetch its files: "+err.Error())
		return
	}
	p := newPusher(c, job, func() { cancel(errLeaseLost) })
	beats := w.heartbeats(shutdown, job, cancel)
	w.logf("run %s (attempt %d): %s", job.ID, job.Attempt, job.Spec.File)
	meta, arts, err := w.cfg.Launcher.Run(ctx, *job, dir, p.add)
	beats()
	p.close()
	switch cause := context.Cause(ctx); {
	case errors.Is(cause, errLeaseLost) || p.lost.Load():
		w.logf("run %s: %v; stopped it", job.ID, errLeaseLost)
		return
	case errors.Is(cause, errShutdown):
		w.release(job)
		return
	}
	if err != nil && meta.Status == "" {
		w.fail(job, "its worker could not run it: "+err.Error())
		return
	}
	var kept []api.Artifact
	for _, path := range arts {
		sum, n, err := c.Upload(context.Background(), path)
		if err != nil {
			w.logf("run %s: artifact %s not uploaded: %v", job.ID, filepath.Base(path), err)
			continue
		}
		kept = append(kept, api.Artifact{Name: filepath.Base(path), SHA256: sum, Bytes: n})
	}
	if err := retry(func() error { return c.Complete(context.Background(), job.ID, job.Lease, meta, kept) }); err != nil {
		w.logf("run %s: its end was not reported: %v", job.ID, err)
		return
	}
	w.logf("run %s: %s", job.ID, meta.Status)
}

// heartbeats renews the job's lease until the returned stop is called, cancelling the run when the
// server cancels it, gives the job to another worker, or the worker shuts down.
func (w *worker) heartbeats(shutdown context.Context, job *api.Job, cancel context.CancelCauseFunc) (stop func()) {
	every := w.cfg.Heartbeat
	if every <= 0 {
		every = time.Duration(job.TTL / 3 * float64(time.Second))
	}
	every = max(every, 50*time.Millisecond)
	done, finished := make(chan struct{}), make(chan struct{})
	go func() {
		defer close(finished)
		t := time.NewTicker(every)
		defer t.Stop()
		for {
			select {
			case <-done:
				return
			case <-shutdown.Done():
				cancel(errShutdown)
				shutdown = context.Background()
			case <-t.C:
				beat, err := w.cfg.Client.Heartbeat(context.Background(), job.ID, job.Lease)
				switch {
				case errors.Is(err, client.ErrLeaseLost):
					cancel(errLeaseLost)
				case err == nil && beat.Cancel:
					cancel(errCancelled)
				}
			}
		}
	}()
	return func() { close(done); <-finished }
}

func (w *worker) release(job *api.Job) {
	err := retry(func() error {
		return w.cfg.Client.Release(context.Background(), job.ID, job.Lease, errShutdown.Error())
	})
	if err != nil {
		w.logf("run %s: not given back: %v (its lease will expire)", job.ID, err)
	}
}

// fail ends a job that could not run, with a record of why.
func (w *worker) fail(job *api.Job, note string) {
	host, _ := os.Hostname()
	m := runs.Meta{ID: job.ID, Status: "failed", ExitCode: 1, Note: note, Host: host}
	if err := retry(func() error { return w.cfg.Client.Complete(context.Background(), job.ID, job.Lease, m, nil) }); err != nil {
		w.logf("run %s: its failure was not reported: %v (its lease will expire)", job.ID, err)
	}
}

// fetch puts a job's file, and its lock, into dir.
func fetch(ctx context.Context, c *client.Client, job *api.Job, dir string) error {
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return err
	}
	file := filepath.Join(dir, filepath.Base(job.Spec.File))
	blobs := map[string]string{file: job.Spec.SHA256}
	if job.Spec.Lock != "" {
		blobs[file+".lock"] = job.Spec.Lock
	}
	for path, sha := range blobs {
		f, err := os.OpenFile(path, os.O_CREATE|os.O_WRONLY|os.O_TRUNC, 0o600)
		if err != nil {
			return err
		}
		err = c.Download(ctx, sha, f)
		if cerr := f.Close(); err == nil {
			err = cerr
		}
		if err != nil {
			return err
		}
	}
	return nil
}

// retry calls fn until it succeeds, the lease is lost, or it has failed for about a minute.
func retry(fn func() error) error {
	backoff := 100 * time.Millisecond
	deadline := time.Now().Add(time.Minute)
	for {
		err := fn()
		var se *client.Error
		if err == nil || errors.Is(err, client.ErrLeaseLost) || (errors.As(err, &se) && se.Code < 500) || time.Now().After(deadline) {
			return err
		}
		time.Sleep(backoff)
		backoff = min(backoff*2, 5*time.Second)
	}
}

// ------------------------------------------------------------------------------- pushing

// pusher sends a run's events to the server in batches, in order, as they are stored. It never
// makes the run wait: events queue, and if the server stays out of reach long enough for a million
// to queue, progress, metrics, resources and output are dropped (and counted) before anything else.
type pusher struct {
	c       *client.Client
	job     *api.Job
	onLost  func()
	mu      sync.Mutex
	buf     []events.Event
	dropped int
	closed  bool
	kick    chan struct{}
	done    chan struct{}
	lost    atomic.Bool
}

const queueCap = 1_000_000

func newPusher(c *client.Client, job *api.Job, onLost func()) *pusher {
	p := &pusher{c: c, job: job, onLost: onLost, kick: make(chan struct{}, 1), done: make(chan struct{})}
	go p.loop()
	return p
}

func (p *pusher) add(e events.Event) {
	p.mu.Lock()
	if len(p.buf) >= queueCap {
		switch e.Kind() {
		case "progress", "metric", "resource", "output":
			p.dropped++
			p.mu.Unlock()
			return
		}
	}
	p.buf = append(p.buf, e)
	n := len(p.buf)
	p.mu.Unlock()
	if n >= 512 {
		select {
		case p.kick <- struct{}{}:
		default:
		}
	}
}

// close sends what is queued and stops.
func (p *pusher) close() {
	p.mu.Lock()
	p.closed = true
	p.mu.Unlock()
	select {
	case p.kick <- struct{}{}:
	default:
	}
	<-p.done
}

func (p *pusher) loop() {
	defer close(p.done)
	t := time.NewTicker(100 * time.Millisecond)
	defer t.Stop()
	for {
		select {
		case <-t.C:
		case <-p.kick:
		}
		for {
			p.mu.Lock()
			batch := p.buf[:min(len(p.buf), 4096)]
			closed := p.closed
			p.mu.Unlock()
			if len(batch) == 0 {
				if closed {
					return
				}
				break
			}
			err := retry(func() error { return p.c.Push(context.Background(), p.job.ID, p.job.Lease, batch) })
			if errors.Is(err, client.ErrLeaseLost) {
				p.lost.Store(true)
				p.onLost()
				p.mu.Lock()
				p.buf = nil
				p.mu.Unlock()
				continue
			}
			p.mu.Lock()
			p.buf = p.buf[len(batch):]
			p.mu.Unlock()
			if err != nil {
				p.mu.Lock()
				p.dropped += len(batch)
				p.mu.Unlock()
				fmt.Fprintf(os.Stderr, "gongoozler worker: %d events of run %s not stored: %v\n", len(batch), p.job.ID, err)
			}
		}
	}
}

func available() int64 {
	n, err := procmem.Available("/proc")
	if err != nil {
		return 0
	}
	return n
}
