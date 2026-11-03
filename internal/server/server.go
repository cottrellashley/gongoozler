// Package server is gongoozler's server (spec/server.md). It keeps runs and the files they need as
// content-addressed blobs, queues each run as a job that a worker leases, and streams each run's
// events, as the worker's supervisor recorded them, to whoever follows it.
//
// A lease lasts TTL past the worker's last heartbeat or push. A lease that expires puts the job back
// in the queue, up to MaxAttempts; after that the run is lost. Only the current lease's events are
// stored, each once: a worker's retried push is recognised by the numbers its supervisor gave.
//
//	DIR/blobs/SHA256           files, locks and artifacts, by content
//	DIR/runs/ID/meta.json      the run's record (the same as a local run's)
//	DIR/runs/ID/spec.json      what was submitted
//	DIR/runs/ID/events.jsonl   its events, numbered `n` in the order the server stored them
//	DIR/runs/ID/artifacts.json what it kept
package server

import (
	"bufio"
	"bytes"
	"crypto/rand"
	"crypto/sha256"
	"crypto/subtle"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/cottrellashley/gongoozler/internal/api"
	"github.com/cottrellashley/gongoozler/internal/events"
	"github.com/cottrellashley/gongoozler/internal/runs"
)

// Config sizes a server.
type Config struct {
	Dir string
	// Token is the bearer token every request must carry; empty for none.
	Token string
	// TTL is how long a lease lasts past the last heartbeat or push. Default 15 s.
	TTL time.Duration
	// MaxAttempts is how many times a job runs whose leases expire. Default 3.
	MaxAttempts int
	Version     string
	// Log is where the server says what went wrong that no request can be told of: a record it
	// could not write. Default: stderr.
	Log io.Writer
}

// Server is the HTTP handler. Close stops its lease reaper.
type Server struct {
	cfg     Config
	mux     *http.ServeMux
	mu      sync.Mutex
	runs    map[string]*run
	queue   []string
	changed chan struct{} // closed and replaced whenever a job may have become leasable
	stop    chan struct{}
	stopped sync.Once
}

type run struct {
	meta      runs.Meta
	spec      api.Spec
	state     string // queued, leased or done
	lease     string
	expires   time.Time
	cancel    bool
	n         int64
	lastWN    int64 // the last worker event number stored under the current lease
	log       *os.File
	updated   chan struct{} // closed and replaced on every new event and state change
	artifacts []api.Artifact
}

// New opens (or creates) a server's directory, puts back in the queue the jobs it held when it last
// stopped, and starts its lease reaper.
func New(cfg Config) (*Server, error) {
	if cfg.TTL == 0 {
		cfg.TTL = 15 * time.Second
	}
	if cfg.MaxAttempts == 0 {
		cfg.MaxAttempts = 3
	}
	if cfg.Version == "" {
		cfg.Version = "dev"
	}
	if cfg.Log == nil {
		cfg.Log = os.Stderr
	}
	for _, d := range []string{filepath.Join(cfg.Dir, "blobs"), filepath.Join(cfg.Dir, "runs")} {
		if err := os.MkdirAll(d, 0o700); err != nil {
			return nil, err
		}
	}
	s := &Server{cfg: cfg, runs: map[string]*run{}, changed: make(chan struct{}), stop: make(chan struct{})}
	if err := s.load(); err != nil {
		return nil, err
	}
	s.routes()
	go s.reaper()
	return s, nil
}

// Close stops the lease reaper and closes the event logs.
func (s *Server) Close() {
	s.stopped.Do(func() {
		close(s.stop)
		s.mu.Lock()
		defer s.mu.Unlock()
		for _, r := range s.runs {
			if r.log != nil {
				s.sync(r)
				r.log.Close()
				r.log = nil
			}
		}
	})
}

func (s *Server) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	if s.cfg.Token != "" {
		got := r.Header.Get("Authorization")
		if subtle.ConstantTimeCompare([]byte(got), []byte("Bearer "+s.cfg.Token)) != 1 {
			w.Header().Set("WWW-Authenticate", `Bearer realm="gongoozler"`)
			fail(w, http.StatusUnauthorized, "a bearer token is required (GONGOOZLER_TOKEN)")
			return
		}
	}
	s.mux.ServeHTTP(w, r)
}

func (s *Server) routes() {
	m := http.NewServeMux()
	m.HandleFunc("GET /v1/health", s.health)
	m.HandleFunc("PUT /v1/blobs/{sha}", s.putBlob)
	m.HandleFunc("GET /v1/blobs/{sha}", s.getBlob)
	m.HandleFunc("POST /v1/runs", s.submit)
	m.HandleFunc("GET /v1/runs", s.list)
	m.HandleFunc("GET /v1/runs/{id}", s.get)
	m.HandleFunc("POST /v1/runs/{id}/cancel", s.cancelRun)
	m.HandleFunc("GET /v1/runs/{id}/events", s.streamEvents)
	m.HandleFunc("GET /v1/runs/{id}/artifacts", s.listArtifacts)
	m.HandleFunc("GET /v1/runs/{id}/artifacts/{name}", s.getArtifact)
	m.HandleFunc("POST /v1/lease", s.leaseJob)
	m.HandleFunc("POST /v1/runs/{id}/heartbeat", s.heartbeat)
	m.HandleFunc("POST /v1/runs/{id}/events", s.pushEvents)
	m.HandleFunc("POST /v1/runs/{id}/complete", s.complete)
	m.HandleFunc("POST /v1/runs/{id}/release", s.release)
	s.mux = m
}

// ------------------------------------------------------------------------------- state

func (s *Server) load() error {
	entries, err := os.ReadDir(filepath.Join(s.cfg.Dir, "runs"))
	if err != nil {
		return err
	}
	for _, d := range entries {
		if !d.IsDir() || !api.ValidID(d.Name()) {
			continue
		}
		id := d.Name()
		meta, err := runs.ReadMeta(s.cfg.Dir, id)
		if err != nil {
			continue
		}
		r := &run{meta: meta, state: "done", updated: make(chan struct{})}
		if err := readJSON(filepath.Join(runs.Dir(s.cfg.Dir, id), "spec.json"), &r.spec); err != nil {
			s.logf("run %s: its spec cannot be read (%v); it is kept, and cannot run again", id, err)
			if meta.Status == "queued" || meta.Status == "running" {
				meta.Status, meta.Note = "lost", "its spec could not be read when the server restarted"
				r.meta = meta
				s.save(r)
			}
		}
		if err := readJSON(filepath.Join(runs.Dir(s.cfg.Dir, id), "artifacts.json"), &r.artifacts); err != nil && !errors.Is(err, os.ErrNotExist) {
			s.logf("run %s: its artifacts' record cannot be read: %v", id, err)
		}
		r.n = lastN(filepath.Join(runs.Dir(s.cfg.Dir, id), "events.jsonl"))
		if r.log, err = os.OpenFile(filepath.Join(runs.Dir(s.cfg.Dir, id), "events.jsonl"), os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0o600); err != nil {
			return err
		}
		s.runs[id] = r
		if r.meta.Status == "queued" || r.meta.Status == "running" {
			r.state, r.meta.Status = "queued", "queued"
			s.queue = append(s.queue, id)
			s.record(r, "job.requeued", map[string]any{"reason": "the server restarted"})
			s.save(r)
		}
	}
	sort.Strings(s.queue)
	return nil
}

func lastN(path string) int64 {
	f, err := os.Open(path)
	if err != nil {
		return 0
	}
	defer f.Close()
	var n int64
	// A line cut short at the end (a crash mid-write) stops the read: numbering goes on after the
	// last whole event.
	_ = events.Read(f, func(e events.Event) error {
		if v, ok := e.Int("n"); ok {
			n = v
		}
		return nil
	})
	return n
}

// record stores an event the server makes itself. The caller holds s.mu.
func (s *Server) record(r *run, kind string, fields map[string]any) {
	e := events.New(kind, fields)
	e["src"] = "server"
	s.store(r, []events.Event{e})
}

// store numbers and appends events, and wakes the run's followers. The caller holds s.mu.
func (s *Server) store(r *run, evs []events.Event) {
	var buf bytes.Buffer
	for _, e := range evs {
		r.n++
		e["n"] = r.n
		b, err := json.Marshal(e)
		if err != nil {
			r.n--
			continue
		}
		buf.Write(b)
		buf.WriteByte('\n')
	}
	if r.log != nil && buf.Len() > 0 {
		if _, err := r.log.Write(buf.Bytes()); err != nil {
			s.logf("run %s: %d events not stored: %v", r.meta.ID, len(evs), err)
		}
	}
	s.touch(r)
}

func (s *Server) logf(format string, args ...any) {
	fmt.Fprintf(s.cfg.Log, "gongoozler server: "+format+"\n", args...)
}

// save writes a run's record. The caller holds s.mu.
func (s *Server) save(r *run) {
	if err := runs.WriteMeta(s.cfg.Dir, r.meta); err != nil {
		s.logf("run %s: its record not written: %v", r.meta.ID, err)
	}
}

// sync makes a run's events durable, as it ends. The caller holds s.mu.
func (s *Server) sync(r *run) {
	if r.log != nil {
		if err := r.log.Sync(); err != nil {
			s.logf("run %s: its events not synced to disk: %v", r.meta.ID, err)
		}
	}
}

func (s *Server) touch(r *run) {
	close(r.updated)
	r.updated = make(chan struct{})
}

func (s *Server) wakeLeasers() {
	close(s.changed)
	s.changed = make(chan struct{})
}

// finish ends a run the server itself must end: one cancelled before it started, or lost. The
// caller holds s.mu.
func (s *Server) finish(r *run, status string, code int, note string) {
	r.state, r.lease = "done", ""
	r.meta.Status, r.meta.ExitCode, r.meta.Note, r.meta.Ended = status, code, note, time.Now()
	r.meta.Wall = r.meta.Ended.Sub(r.meta.Started).Seconds()
	s.record(r, "run.end", map[string]any{"id": r.meta.ID, "status": status, "exit_code": code, "note": note, "wall": r.meta.Wall, "peak": r.meta.Peak})
	r.meta.Events = r.n
	s.save(r)
	s.sync(r)
}

// requeue puts a leased job back in the queue, or ends it when it has had its attempts. The caller
// holds s.mu.
func (s *Server) requeue(r *run, why string) {
	switch {
	case r.cancel:
		s.finish(r, "cancelled", 143, "cancelled; "+why+" before it stopped")
	case r.meta.Attempt >= s.cfg.MaxAttempts:
		s.finish(r, "lost", 1, fmt.Sprintf("%s, on each of %d attempts", why, r.meta.Attempt))
	default:
		r.state, r.lease, r.meta.Status = "queued", "", "queued"
		s.queue = append(s.queue, r.meta.ID)
		s.record(r, "job.requeued", map[string]any{"reason": why, "attempt": r.meta.Attempt})
		s.save(r)
		s.wakeLeasers()
	}
}

// reaper expires leases whose worker has gone quiet.
func (s *Server) reaper() {
	t := time.NewTicker(max(s.cfg.TTL/4, 10*time.Millisecond))
	defer t.Stop()
	for {
		select {
		case <-s.stop:
			return
		case <-t.C:
		}
		s.mu.Lock()
		now := time.Now()
		for _, r := range s.runs {
			if r.state != "leased" || now.Before(r.expires) {
				continue
			}
			s.record(r, "job.expired", map[string]any{"worker": r.meta.Worker, "attempt": r.meta.Attempt})
			s.requeue(r, "its worker went quiet")
		}
		s.mu.Unlock()
	}
}

func (s *Server) find(w http.ResponseWriter, req *http.Request) *run {
	id := req.PathValue("id")
	s.mu.Lock()
	r := s.runs[id]
	s.mu.Unlock()
	if r == nil {
		fail(w, http.StatusNotFound, "no run "+strconv.Quote(id))
	}
	return r
}

// ------------------------------------------------------------------------------- handlers

func (s *Server) health(w http.ResponseWriter, _ *http.Request) {
	s.mu.Lock()
	counts := map[string]int{}
	for _, r := range s.runs {
		counts[r.state]++
	}
	s.mu.Unlock()
	reply(w, http.StatusOK, map[string]any{"version": s.cfg.Version, "queued": counts["queued"], "running": counts["leased"], "done": counts["done"]})
}

func (s *Server) blobPath(sha string) string { return filepath.Join(s.cfg.Dir, "blobs", sha) }

func (s *Server) hasBlob(sha string) bool {
	_, err := os.Stat(s.blobPath(sha))
	return err == nil
}

func (s *Server) putBlob(w http.ResponseWriter, r *http.Request) {
	sha := r.PathValue("sha")
	if !api.ValidSHA256(sha) {
		fail(w, http.StatusBadRequest, "a blob is named by its SHA-256, in lowercase hex")
		return
	}
	if s.hasBlob(sha) {
		_, _ = io.Copy(io.Discard, r.Body) // already stored: the upload is not needed
		reply(w, http.StatusOK, map[string]any{"sha256": sha, "stored": false})
		return
	}
	tmp, err := os.CreateTemp(filepath.Join(s.cfg.Dir, "blobs"), ".upload-")
	if err != nil {
		fail(w, http.StatusInternalServerError, err.Error())
		return
	}
	defer os.Remove(tmp.Name())
	h := sha256.New()
	n, err := io.Copy(io.MultiWriter(tmp, h), r.Body)
	if cerr := tmp.Close(); err == nil {
		err = cerr
	}
	if err != nil {
		fail(w, http.StatusBadRequest, "the upload broke off: "+err.Error())
		return
	}
	if got := hex.EncodeToString(h.Sum(nil)); got != sha {
		fail(w, http.StatusBadRequest, fmt.Sprintf("the content's SHA-256 is %s, not %s", got, sha))
		return
	}
	if err := os.Rename(tmp.Name(), s.blobPath(sha)); err != nil {
		fail(w, http.StatusInternalServerError, err.Error())
		return
	}
	reply(w, http.StatusCreated, map[string]any{"sha256": sha, "bytes": n, "stored": true})
}

func (s *Server) getBlob(w http.ResponseWriter, r *http.Request) {
	sha := r.PathValue("sha")
	if !api.ValidSHA256(sha) || !s.hasBlob(sha) {
		fail(w, http.StatusNotFound, "no blob "+sha)
		return
	}
	w.Header().Set("Content-Type", "application/octet-stream")
	http.ServeFile(w, r, s.blobPath(sha))
}

func (s *Server) submit(w http.ResponseWriter, req *http.Request) {
	var spec api.Spec
	if err := decode(req, &spec, 1<<20); err != nil {
		fail(w, http.StatusBadRequest, err.Error())
		return
	}
	name := filepath.Base(spec.File)
	switch {
	case name == "." || name == "/" || name == "" || strings.HasPrefix(name, "."):
		fail(w, http.StatusBadRequest, "a run needs its file's name")
		return
	case !api.ValidSHA256(spec.SHA256) || !s.hasBlob(spec.SHA256):
		fail(w, http.StatusBadRequest, "upload the file as a blob before submitting it")
		return
	case spec.Lock != "" && (!api.ValidSHA256(spec.Lock) || !s.hasBlob(spec.Lock)):
		fail(w, http.StatusBadRequest, "upload the lock as a blob before submitting it")
		return
	case spec.Memory < 0 || spec.CPUs < 0 || spec.Timeout < 0:
		fail(w, http.StatusBadRequest, "memory, cpus and timeout cannot be negative")
		return
	}
	spec.File = name
	if spec.Args == nil {
		spec.Args = []string{}
	}
	if spec.Params == nil {
		spec.Params = map[string]any{}
	}
	now := time.Now()
	id := runs.NewID(now)
	meta := runs.Meta{ID: id, File: name, SHA256: spec.SHA256, Lock: spec.Lock, Args: spec.Args, Params: spec.Params, Started: now,
		Status: "queued", Placement: "remote", Sweep: spec.Sweep, Version: s.cfg.Version}
	dir := runs.Dir(s.cfg.Dir, id)
	if err := os.MkdirAll(dir, 0o700); err != nil {
		fail(w, http.StatusInternalServerError, err.Error())
		return
	}
	if err := writeJSON(filepath.Join(dir, "spec.json"), spec); err != nil {
		fail(w, http.StatusInternalServerError, err.Error())
		return
	}
	log, err := os.OpenFile(filepath.Join(dir, "events.jsonl"), os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0o600)
	if err != nil {
		fail(w, http.StatusInternalServerError, err.Error())
		return
	}
	r := &run{meta: meta, spec: spec, state: "queued", log: log, updated: make(chan struct{})}
	s.mu.Lock()
	s.runs[id] = r
	s.queue = append(s.queue, id)
	s.record(r, "job.queued", map[string]any{"id": id, "file": name, "params": spec.Params, "memory": spec.Memory, "cpus": spec.CPUs})
	s.save(r)
	s.wakeLeasers()
	m := r.meta
	s.mu.Unlock()
	reply(w, http.StatusCreated, m)
}

func (s *Server) list(w http.ResponseWriter, _ *http.Request) {
	s.mu.Lock()
	out := make([]runs.Meta, 0, len(s.runs))
	for _, r := range s.runs {
		out = append(out, r.meta)
	}
	s.mu.Unlock()
	sort.Slice(out, func(i, j int) bool { return out[i].ID < out[j].ID })
	reply(w, http.StatusOK, out)
}

func (s *Server) get(w http.ResponseWriter, req *http.Request) {
	if r := s.find(w, req); r != nil {
		s.mu.Lock()
		m := r.meta
		s.mu.Unlock()
		reply(w, http.StatusOK, m)
	}
}

func (s *Server) cancelRun(w http.ResponseWriter, req *http.Request) {
	r := s.find(w, req)
	if r == nil {
		return
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	switch r.state {
	case "queued":
		for i, id := range s.queue {
			if id == r.meta.ID {
				s.queue = append(s.queue[:i], s.queue[i+1:]...)
				break
			}
		}
		s.finish(r, "cancelled", 143, "cancelled before it started")
	case "leased":
		if !r.cancel {
			r.cancel = true
			s.record(r, "job.cancel", map[string]any{"worker": r.meta.Worker})
		}
	}
	reply(w, http.StatusAccepted, r.meta)
}

func (s *Server) listArtifacts(w http.ResponseWriter, req *http.Request) {
	if r := s.find(w, req); r != nil {
		s.mu.Lock()
		out := append([]api.Artifact{}, r.artifacts...)
		s.mu.Unlock()
		reply(w, http.StatusOK, out)
	}
}

func (s *Server) getArtifact(w http.ResponseWriter, req *http.Request) {
	r := s.find(w, req)
	if r == nil {
		return
	}
	name := req.PathValue("name")
	s.mu.Lock()
	var sha string
	for _, a := range r.artifacts {
		if a.Name == name {
			sha = a.SHA256
		}
	}
	s.mu.Unlock()
	if sha == "" || !s.hasBlob(sha) {
		fail(w, http.StatusNotFound, "run "+r.meta.ID+" kept no artifact "+strconv.Quote(name))
		return
	}
	w.Header().Set("Content-Type", "application/octet-stream")
	w.Header().Set("X-Gongoozler-SHA256", sha)
	http.ServeFile(w, req, s.blobPath(sha))
}

// streamEvents sends a run's events after Last-Event-ID (or ?after=N): as server-sent events,
// following the run until it ends with an `end` event; or, with ?follow=0, as JSON lines of what
// is stored now.
func (s *Server) streamEvents(w http.ResponseWriter, req *http.Request) {
	r := s.find(w, req)
	if r == nil {
		return
	}
	after, _ := strconv.ParseInt(req.Header.Get("Last-Event-ID"), 10, 64)
	if q := req.URL.Query().Get("after"); q != "" {
		after, _ = strconv.ParseInt(q, 10, 64)
	}
	f, err := os.Open(filepath.Join(runs.Dir(s.cfg.Dir, r.meta.ID), "events.jsonl"))
	if err != nil {
		fail(w, http.StatusInternalServerError, err.Error())
		return
	}
	defer f.Close()
	t := &tail{r: bufio.NewReaderSize(f, 1<<16)}
	if req.URL.Query().Get("follow") == "0" {
		w.Header().Set("Content-Type", "application/x-ndjson")
		t.each(func(n int64, line []byte) {
			if n > after {
				w.Write(line)
				w.Write([]byte("\n"))
			}
		})
		return
	}
	flusher, ok := w.(http.Flusher)
	if !ok {
		fail(w, http.StatusInternalServerError, "streaming is not supported here")
		return
	}
	w.Header().Set("Content-Type", "text/event-stream")
	w.Header().Set("Cache-Control", "no-cache")
	w.Header().Set("X-Accel-Buffering", "no")
	w.WriteHeader(http.StatusOK)
	ping := time.NewTicker(15 * time.Second)
	defer ping.Stop()
	for {
		s.mu.Lock()
		changed, done, status, last := r.updated, r.state == "done", r.meta.Status, r.n
		s.mu.Unlock()
		t.each(func(n int64, line []byte) {
			if n > after {
				fmt.Fprintf(w, "id: %d\ndata: %s\n\n", n, line)
				after = n
			}
		})
		if done && t.last >= last {
			fmt.Fprintf(w, "event: end\ndata: {\"status\":%q}\n\n", status)
			flusher.Flush()
			return
		}
		flusher.Flush()
		select {
		case <-changed:
		case <-ping.C:
			fmt.Fprint(w, ": ping\n\n")
			flusher.Flush()
		case <-req.Context().Done():
			return
		case <-s.stop:
			return
		}
	}
}

// tail reads a growing event log, a whole line at a time.
type tail struct {
	r       *bufio.Reader
	partial []byte
	last    int64
}

func (t *tail) each(fn func(n int64, line []byte)) {
	for {
		chunk, err := t.r.ReadBytes('\n')
		if len(chunk) > 0 {
			t.partial = append(t.partial, chunk...)
		}
		if err != nil {
			return // the rest of a line not yet written stays for the next read
		}
		line := bytes.TrimSuffix(t.partial, []byte("\n"))
		t.partial = nil
		var head struct {
			N int64 `json:"n"`
		}
		if json.Unmarshal(line, &head) == nil {
			t.last = head.N
			fn(head.N, line)
		}
	}
}

// leaseJob gives the worker the oldest queued job that fits its offer, waiting up to the offer's
// wait (at most a minute) for one.
func (s *Server) leaseJob(w http.ResponseWriter, req *http.Request) {
	var o api.Offer
	if err := decode(req, &o, 1<<16); err != nil {
		fail(w, http.StatusBadRequest, err.Error())
		return
	}
	if o.Worker == "" {
		fail(w, http.StatusBadRequest, "a worker names itself")
		return
	}
	deadline := time.After(time.Duration(min(max(o.Wait, 0), 60) * float64(time.Second)))
	for {
		s.mu.Lock()
		job, changed := s.take(o), s.changed
		s.mu.Unlock()
		if job != nil {
			reply(w, http.StatusOK, job)
			return
		}
		select {
		case <-changed:
		case <-deadline:
			w.WriteHeader(http.StatusNoContent)
			return
		case <-req.Context().Done():
			return
		case <-s.stop:
			w.WriteHeader(http.StatusNoContent)
			return
		}
	}
}

// take leases the first queued job that fits the offer. The caller holds s.mu.
func (s *Server) take(o api.Offer) *api.Job {
	for i, id := range s.queue {
		r := s.runs[id]
		if (r.spec.Memory > 0 && o.Memory < r.spec.Memory) || (r.spec.CPUs > 0 && o.CPUs < r.spec.CPUs) {
			continue
		}
		s.queue = append(s.queue[:i], s.queue[i+1:]...)
		b := make([]byte, 16)
		rand.Read(b)
		r.state, r.lease, r.expires, r.lastWN = "leased", hex.EncodeToString(b), time.Now().Add(s.cfg.TTL), 0
		r.meta.Status, r.meta.Worker, r.meta.Attempt = "running", o.Worker, r.meta.Attempt+1
		s.record(r, "job.leased", map[string]any{"worker": o.Worker, "attempt": r.meta.Attempt})
		s.save(r)
		return &api.Job{ID: id, Lease: r.lease, Attempt: r.meta.Attempt, TTL: s.cfg.TTL.Seconds(), Spec: r.spec}
	}
	return nil
}

// leased finds the run a worker's request is about, if the worker still holds its lease, and
// renews the lease. It answers 409 when the lease is no longer the worker's. On success it returns
// with s.mu held.
func (s *Server) leased(w http.ResponseWriter, req *http.Request) *run {
	r := s.find(w, req)
	if r == nil {
		return nil
	}
	lease := req.Header.Get(api.LeaseHeader)
	s.mu.Lock()
	if r.state != "leased" || lease == "" || subtle.ConstantTimeCompare([]byte(lease), []byte(r.lease)) != 1 {
		s.mu.Unlock()
		fail(w, http.StatusConflict, "this lease is no longer held: stop the run")
		return nil
	}
	r.expires = time.Now().Add(s.cfg.TTL)
	return r
}

func (s *Server) heartbeat(w http.ResponseWriter, req *http.Request) {
	r := s.leased(w, req)
	if r == nil {
		return
	}
	beat := api.Beat{Cancel: r.cancel}
	s.mu.Unlock()
	reply(w, http.StatusOK, beat)
}

// pushEvents stores the events a worker's supervisor recorded, as JSON lines, skipping any it
// pushed before (by the supervisor's numbering).
func (s *Server) pushEvents(w http.ResponseWriter, req *http.Request) {
	body, err := io.ReadAll(http.MaxBytesReader(w, req.Body, 64<<20))
	if err != nil {
		fail(w, http.StatusRequestEntityTooLarge, err.Error())
		return
	}
	var evs []events.Event
	sc := bufio.NewScanner(bytes.NewReader(body))
	sc.Buffer(make([]byte, 0, 1<<16), events.MaxLine)
	for sc.Scan() {
		d := json.NewDecoder(bytes.NewReader(sc.Bytes()))
		d.UseNumber()
		var e events.Event
		if err := d.Decode(&e); err != nil || e == nil {
			fail(w, http.StatusBadRequest, "not an event: "+strconv.Quote(string(sc.Bytes())))
			return
		}
		evs = append(evs, e)
	}
	r := s.leased(w, req)
	if r == nil {
		return
	}
	defer s.mu.Unlock()
	var fresh []events.Event
	for _, e := range evs {
		wn, ok := e.Int("n")
		if !ok || wn <= r.lastWN {
			continue
		}
		r.lastWN = wn
		e["wn"] = wn
		if r.meta.Attempt > 1 {
			e["attempt"] = r.meta.Attempt
		}
		fresh = append(fresh, e)
	}
	s.store(r, fresh)
	reply(w, http.StatusOK, map[string]any{"stored": len(fresh), "last": r.lastWN})
}

// release takes a job back from a worker that is shutting down, to run again elsewhere.
func (s *Server) release(w http.ResponseWriter, req *http.Request) {
	var body struct {
		Reason string `json:"reason"`
	}
	_ = decode(req, &body, 1<<16) // the reason is for the record; a release needs none
	r := s.leased(w, req)
	if r == nil {
		return
	}
	defer s.mu.Unlock()
	s.record(r, "job.released", map[string]any{"worker": r.meta.Worker, "attempt": r.meta.Attempt, "reason": body.Reason})
	s.requeue(r, "its worker gave it back")
	reply(w, http.StatusOK, r.meta)
}

// Completion is what a worker sends when a run has ended.
type Completion struct {
	Meta      runs.Meta      `json:"meta"`
	Artifacts []api.Artifact `json:"artifacts"`
}

func (s *Server) complete(w http.ResponseWriter, req *http.Request) {
	var c Completion
	if err := decode(req, &c, 16<<20); err != nil {
		fail(w, http.StatusBadRequest, err.Error())
		return
	}
	for _, a := range c.Artifacts {
		if !api.ValidSHA256(a.SHA256) || !s.hasBlob(a.SHA256) {
			fail(w, http.StatusBadRequest, "upload artifact "+strconv.Quote(a.Name)+" as a blob before completing")
			return
		}
	}
	r := s.leased(w, req)
	if r == nil {
		return
	}
	defer s.mu.Unlock()
	m := &r.meta
	got := c.Meta
	m.Status, m.ExitCode, m.Signal, m.Note, m.Wall, m.Peak = got.Status, got.ExitCode, got.Signal, got.Note, got.Wall, got.Peak
	m.MemorySource, m.OOMKills, m.Leftovers, m.Telemetry, m.Host = got.MemorySource, got.OOMKills, got.Leftovers, got.Telemetry, got.Host
	if got.Version != "" {
		m.Version = got.Version
	}
	if m.Status == "" || m.Status == "running" {
		m.Status = "failed"
	}
	m.Ended, m.Events = time.Now(), r.n
	r.state, r.lease, r.artifacts = "done", "", c.Artifacts
	if err := writeJSON(filepath.Join(runs.Dir(s.cfg.Dir, m.ID), "artifacts.json"), c.Artifacts); err != nil {
		s.logf("run %s: its artifacts' record not written: %v", m.ID, err)
	}
	s.save(r)
	s.sync(r)
	s.touch(r)
	reply(w, http.StatusOK, *m)
}

// ------------------------------------------------------------------------------- helpers

func decode(req *http.Request, v any, limit int64) error {
	d := json.NewDecoder(io.LimitReader(req.Body, limit))
	d.UseNumber()
	if err := d.Decode(v); err != nil {
		return fmt.Errorf("the request is not the JSON expected: %w", err)
	}
	return nil
}

func reply(w http.ResponseWriter, code int, v any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(code)
	json.NewEncoder(w).Encode(v)
}

func fail(w http.ResponseWriter, code int, msg string) {
	reply(w, code, map[string]string{"error": msg})
}

func readJSON(path string, v any) error {
	b, err := os.ReadFile(path)
	if err != nil {
		return err
	}
	d := json.NewDecoder(bytes.NewReader(b))
	d.UseNumber()
	return d.Decode(v)
}

func writeJSON(path string, v any) error {
	b, err := json.MarshalIndent(v, "", "  ")
	if err != nil {
		return err
	}
	tmp := path + ".tmp"
	if err := os.WriteFile(tmp, append(b, '\n'), 0o600); err != nil {
		return err
	}
	return os.Rename(tmp, path)
}
