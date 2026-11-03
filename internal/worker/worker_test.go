package worker

import (
	"archive/tar"
	"bytes"
	"context"
	"encoding/binary"
	"encoding/json"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/cottrellashley/gongoozler/internal/api"
	"github.com/cottrellashley/gongoozler/internal/client"
	"github.com/cottrellashley/gongoozler/internal/events"
	"github.com/cottrellashley/gongoozler/internal/runs"
	"github.com/cottrellashley/gongoozler/internal/server"
)

func pythonSrc() string {
	_, here, _, _ := runtime.Caller(0)
	return filepath.Join(filepath.Dir(here), "..", "..", "python", "src")
}

type lockedLog struct {
	mu sync.Mutex
	b  bytes.Buffer
}

func (l *lockedLog) Write(p []byte) (int, error) {
	l.mu.Lock()
	defer l.mu.Unlock()
	return l.b.Write(p)
}
func (l *lockedLog) String() string { l.mu.Lock(); defer l.mu.Unlock(); return l.b.String() }

func newServer(t *testing.T, cfg server.Config) *client.Client {
	t.Helper()
	cfg.Dir = t.TempDir()
	s, err := server.New(cfg)
	if err != nil {
		t.Fatal(err)
	}
	hs := httptest.NewServer(s)
	t.Cleanup(func() { s.Close(); hs.Close() })
	return client.New(hs.URL, "")
}

// startWorker runs a worker with the in-process launcher until the test ends, or until the
// returned stop is called.
func startWorker(t *testing.T, c *client.Client, name string, heartbeat time.Duration) (*lockedLog, func()) {
	t.Helper()
	log := &lockedLog{}
	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan struct{})
	cfg := Config{Client: c, Name: name, Work: t.TempDir(), Heartbeat: heartbeat, Wait: 200 * time.Millisecond, Log: log,
		Launcher: Process{Launcher: []string{"python3"}, Env: []string{"PYTHONPATH=" + pythonSrc()}, Cgroup: "off", Version: "test"}}
	go func() {
		if err := Run(ctx, cfg); err != nil {
			t.Error(err)
		}
		close(done)
	}()
	stop := func() {
		cancel()
		select {
		case <-done:
		case <-time.After(20 * time.Second):
			t.Error("the worker did not stop")
		}
	}
	t.Cleanup(stop)
	return log, stop
}

func submit(t *testing.T, c *client.Client, code string, params map[string]any) runs.Meta {
	t.Helper()
	p := filepath.Join(t.TempDir(), "idea.py")
	if err := os.WriteFile(p, []byte(code), 0o600); err != nil {
		t.Fatal(err)
	}
	sum, _, err := c.Upload(context.Background(), p)
	if err != nil {
		t.Fatal(err)
	}
	m, err := c.Submit(context.Background(), api.Spec{File: "idea.py", SHA256: sum, Params: params})
	if err != nil {
		t.Fatal(err)
	}
	return m
}

func waitStatus(t *testing.T, c *client.Client, id string, want ...string) runs.Meta {
	t.Helper()
	deadline := time.Now().Add(30 * time.Second)
	for {
		m, err := c.Run(context.Background(), id)
		if err == nil {
			for _, w := range want {
				if m.Status == w {
					return m
				}
			}
		}
		if time.Now().After(deadline) {
			t.Fatalf("run %s is %s, want %v", id, m.Status, want)
		}
		time.Sleep(20 * time.Millisecond)
	}
}

func kindsOf(t *testing.T, c *client.Client, id string) []string {
	t.Helper()
	var k []string
	if err := c.Events(context.Background(), id, 0, func(e events.Event) error { k = append(k, e.Kind()); return nil }); err != nil {
		t.Fatal(err)
	}
	return k
}

func TestAWorkerRunsAJobAndTheServerKeepsItsEventsAndArtifacts(t *testing.T) {
	c := newServer(t, server.Config{})
	startWorker(t, c, "w1", 100*time.Millisecond)
	m := submit(t, c, `import gongoozler as gz
from gongoozler import results
p = gz.params(x=1)
with gz.stage("work"):
    gz.progress("steps", 1, 1)
results.write_json("stats.json", {"x": p.x})
print("printed")
`, map[string]any{"x": 7})
	var seen []string
	status, err := c.Follow(context.Background(), m.ID, 0, func(e events.Event) error { seen = append(seen, e.Kind()); return nil })
	if err != nil || status != "ok" {
		t.Fatalf("followed to %q: %v (%v)", status, err, seen)
	}
	all := strings.Join(seen, " ")
	for _, want := range []string{"job.queued", "job.leased", "run.start", "hello", "params", "stage", "output", "artifact", "run.end"} {
		if !strings.Contains(all, want) {
			t.Fatalf("no %s in %s", want, all)
		}
	}
	got, _ := c.Run(context.Background(), m.ID)
	if got.ExitCode != 0 || got.Worker != "w1" || got.Host == "" || got.Wall <= 0 {
		t.Fatalf("meta %+v", got)
	}
	var buf bytes.Buffer
	if err := c.Artifact(context.Background(), m.ID, "stats.json", &buf); err != nil || strings.TrimSpace(buf.String()) != `{"x":7}` {
		t.Fatalf("stats.json: %v %q", err, buf.String())
	}
}

func TestACancelledJobIsStoppedOnItsWorker(t *testing.T) {
	c := newServer(t, server.Config{})
	startWorker(t, c, "w1", 50*time.Millisecond)
	m := submit(t, c, "import time, gongoozler as gz\nwith gz.stage('wait'):\n    time.sleep(60)\n", nil)
	waitStatus(t, c, m.ID, "running")
	time.Sleep(300 * time.Millisecond)
	t0 := time.Now()
	if _, err := c.Cancel(context.Background(), m.ID); err != nil {
		t.Fatal(err)
	}
	got := waitStatus(t, c, m.ID, "cancelled")
	if time.Since(t0) > 10*time.Second || !strings.Contains(strings.Join(kindsOf(t, c, m.ID), " "), "job.cancel") {
		t.Fatalf("cancelled after %v: %+v", time.Since(t0), got)
	}
}

func TestAWorkerThatLosesItsLeaseStopsTheRunAndDoesNotReportIt(t *testing.T) {
	c := newServer(t, server.Config{TTL: 150 * time.Millisecond, MaxAttempts: 1})
	log, _ := startWorker(t, c, "w1", 2*time.Second)
	m := submit(t, c, "import time\ntime.sleep(60)\n", nil)
	got := waitStatus(t, c, m.ID, "lost")
	if !strings.Contains(got.Note, "worker went quiet") {
		t.Fatalf("note %q", got.Note)
	}
	deadline := time.Now().Add(10 * time.Second)
	for !strings.Contains(log.String(), "the server gave the job to another worker; stopped it") {
		if time.Now().After(deadline) {
			t.Fatalf("the worker did not stop the run:\n%s", log.String())
		}
		time.Sleep(20 * time.Millisecond)
	}
	if after, _ := c.Run(context.Background(), m.ID); after.Status != "lost" {
		t.Fatalf("the worker reported a run it no longer held: %s", after.Status)
	}
}

func TestAWorkerShuttingDownGivesItsJobsBackToRunElsewhere(t *testing.T) {
	c := newServer(t, server.Config{})
	_, stop := startWorker(t, c, "w1", 50*time.Millisecond)
	m := submit(t, c, "import time, gongoozler as gz\np = gz.params(seconds=60.0)\ntime.sleep(p.seconds)\n", map[string]any{"seconds": 2.0})
	waitStatus(t, c, m.ID, "running")
	time.Sleep(200 * time.Millisecond)
	stop()
	startWorker(t, c, "w2", 50*time.Millisecond)
	got := waitStatus(t, c, m.ID, "ok", "failed", "cancelled", "lost")
	if got.Status != "ok" || got.Worker != "w2" || got.Attempt != 2 {
		t.Fatalf("after the hand-back: %+v", got)
	}
	k := strings.Join(kindsOf(t, c, m.ID), " ")
	if !strings.Contains(k, "job.released") || !strings.Contains(k, "job.requeued") {
		t.Fatalf("events: %s", k)
	}
}

// ------------------------------------------------------------------------------- Docker

// fakeDocker is the part of the Docker Engine API a box uses, on a unix socket. Its container
// "runs" by replaying a supervisor's JSON events and then holding a run directory to archive.
type fakeDocker struct {
	mu      sync.Mutex
	created ContainerSpec
	files   map[string]string
	killed  []string
	removed bool
	out     []byte // the container's stdout
	state   map[string]string
	oom     bool
	exit    int
	hold    chan struct{} // closed when the container exits
}

func (f *fakeDocker) handler(t *testing.T) http.Handler {
	m := http.NewServeMux()
	// A daemon says which API version it speaks; this one, 1.47 (the paths below are 1.47's).
	m.HandleFunc("GET /_ping", func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Api-Version", "1.47")
		w.Write([]byte("OK"))
	})
	m.HandleFunc("POST /v1.47/containers/create", func(w http.ResponseWriter, r *http.Request) {
		f.mu.Lock()
		if err := json.NewDecoder(r.Body).Decode(&f.created); err != nil {
			t.Errorf("create: %v", err)
		}
		f.mu.Unlock()
		w.WriteHeader(http.StatusCreated)
		w.Write([]byte(`{"Id": "c0ffee"}`))
	})
	m.HandleFunc("PUT /v1.47/containers/c0ffee/archive", func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Query().Get("path") != "/job" {
			t.Errorf("files put in %s", r.URL.Query().Get("path"))
		}
		tr := tar.NewReader(r.Body)
		f.mu.Lock()
		f.files = map[string]string{}
		for {
			h, err := tr.Next()
			if err != nil {
				break
			}
			b, _ := io.ReadAll(tr)
			f.files[h.Name] = string(b)
		}
		f.mu.Unlock()
	})
	m.HandleFunc("POST /v1.47/containers/c0ffee/start", func(w http.ResponseWriter, r *http.Request) { w.WriteHeader(http.StatusNoContent) })
	m.HandleFunc("GET /v1.47/containers/c0ffee/logs", func(w http.ResponseWriter, r *http.Request) {
		frame := func(stream byte, p []byte) {
			var hdr [8]byte
			hdr[0] = stream
			binary.BigEndian.PutUint32(hdr[4:], uint32(len(p)))
			w.Write(hdr[:])
			w.Write(p)
		}
		// Split the stdout across frames anywhere, as the daemon may.
		for i := 0; i < len(f.out); i += 37 {
			frame(1, f.out[i:min(i+37, len(f.out))])
		}
		frame(2, []byte("a line on stderr\n"))
		w.(http.Flusher).Flush()
		<-f.hold
	})
	m.HandleFunc("POST /v1.47/containers/c0ffee/wait", func(w http.ResponseWriter, r *http.Request) {
		<-f.hold
		json.NewEncoder(w).Encode(map[string]any{"StatusCode": f.exit})
	})
	m.HandleFunc("GET /v1.47/containers/c0ffee/json", func(w http.ResponseWriter, r *http.Request) {
		json.NewEncoder(w).Encode(map[string]any{"State": map[string]any{"OOMKilled": f.oom}})
	})
	m.HandleFunc("GET /v1.47/containers/c0ffee/archive", func(w http.ResponseWriter, r *http.Request) {
		if f.state == nil {
			http.Error(w, `{"message": "Could not find the file /state/runs"}`, http.StatusNotFound)
			return
		}
		tw := tar.NewWriter(w)
		for name, content := range f.state {
			if tw.WriteHeader(&tar.Header{Name: name, Mode: 0o600, Size: int64(len(content)), Typeflag: tar.TypeReg}) != nil {
				return
			}
			if _, err := tw.Write([]byte(content)); err != nil {
				return
			}
		}
		_ = tw.Close() // a client gone mid-archive sees it cut short
	})
	m.HandleFunc("POST /v1.47/containers/c0ffee/kill", func(w http.ResponseWriter, r *http.Request) {
		f.mu.Lock()
		f.killed = append(f.killed, r.URL.Query().Get("signal"))
		f.mu.Unlock()
		select {
		case <-f.hold:
		default:
			close(f.hold)
		}
		w.WriteHeader(http.StatusNoContent)
	})
	m.HandleFunc("DELETE /v1.47/containers/c0ffee", func(w http.ResponseWriter, r *http.Request) {
		f.mu.Lock()
		f.removed = true
		f.mu.Unlock()
		w.WriteHeader(http.StatusNoContent)
	})
	return m
}

func serveDocker(t *testing.T, f *fakeDocker) string {
	t.Helper()
	sock := filepath.Join(t.TempDir(), "docker.sock")
	ln, err := net.Listen("unix", sock)
	if err != nil {
		t.Fatal(err)
	}
	hs := &http.Server{Handler: f.handler(t)}
	go func() { _ = hs.Serve(ln) }() // ends with ErrServerClosed at cleanup
	t.Cleanup(func() { _ = hs.Close() })
	return "unix://" + sock
}

func eventLines(evs ...events.Event) []byte {
	var b bytes.Buffer
	for _, e := range evs {
		j, _ := json.Marshal(e)
		b.Write(append(j, '\n'))
	}
	return b.Bytes()
}

func numbered(kind string, n int, fields map[string]any) events.Event {
	e := events.New(kind, fields)
	e["n"], e["seq"] = n, n
	return e
}

func jobFor(dir string) api.Job {
	for name, content := range map[string]string{"idea.py": "print('hi')\n", "idea.py.lock": "version = 1\n"} {
		if err := os.WriteFile(filepath.Join(dir, name), []byte(content), 0o600); err != nil {
			panic(err)
		}
	}
	return api.Job{ID: "20261103T120000Z-0000beef", Attempt: 1, Spec: api.Spec{File: "idea.py", Args: []string{"--fast"}, Params: map[string]any{"x": 2},
		Memory: 8 << 30, CPUs: 2, Timeout: 3600}}
}

func TestADockerBoxRunsTheSupervisorInsideAndItsEventsComeOut(t *testing.T) {
	f := &fakeDocker{hold: make(chan struct{}), out: eventLines(
		numbered("run.start", 1, map[string]any{"id": "x"}), numbered("hello", 2, nil), numbered("run.end", 3, map[string]any{"status": "ok"})),
		state: map[string]string{
			"20261103T120000Z-0000beef/meta.json":            `{"id": "20261103T120000Z-0000beef", "status": "ok", "exit_code": 0, "wall": 1.5}`,
			"20261103T120000Z-0000beef/artifacts/stats.json": `{"x": 2}`,
			"20261103T120000Z-0000beef/events.jsonl":         "",
		}}
	close(f.hold)
	d := &Docker{Host: serveDocker(t, f), Image: "gongoozler:test"}
	dir := t.TempDir()
	var got []string
	meta, arts, err := d.Run(context.Background(), jobFor(dir), dir, func(e events.Event) { got = append(got, e.Kind()) })
	if err != nil {
		t.Fatal(err)
	}
	if strings.Join(got, " ") != "run.start hello run.end" {
		t.Fatalf("events %v", got)
	}
	if meta.Status != "ok" || meta.Wall != 1.5 || len(arts) != 1 || filepath.Base(arts[0]) != "stats.json" {
		t.Fatalf("meta %+v, artifacts %v", meta, arts)
	}
	if b, _ := os.ReadFile(arts[0]); string(b) != `{"x": 2}` {
		t.Fatalf("stats.json %q", b)
	}
	c := f.created
	cmd := strings.Join(append(append([]string{}, c.Entrypoint...), c.Cmd...), " ")
	if cmd != "gongoozler run --json --local --no-lock --id 20261103T120000Z-0000beef --params-json {\"x\":2} --timeout 1h0m0s /job/idea.py --fast" {
		t.Fatalf("command %q", cmd)
	}
	if c.Image != "gongoozler:test" || c.HostConfig.Memory != 8<<30 || c.HostConfig.MemorySwap != 8<<30 || c.HostConfig.NanoCPUs != 2e9 || !c.HostConfig.Init {
		t.Fatalf("created %+v", c)
	}
	if f.files["idea.py"] != "print('hi')\n" || f.files["idea.py.lock"] != "version = 1\n" || !f.removed {
		t.Fatalf("files %v, removed %v", f.files, f.removed)
	}
}

func TestACancelledBoxIsToldAsKillWouldTellALocalRun(t *testing.T) {
	f := &fakeDocker{hold: make(chan struct{}), out: eventLines(numbered("run.start", 1, nil))}
	d := &Docker{Host: serveDocker(t, f), Image: "gongoozler:test", Grace: time.Second}
	dir := t.TempDir()
	ctx, cancel := context.WithCancel(context.Background())
	time.AfterFunc(200*time.Millisecond, cancel)
	var got []events.Event
	meta, _, err := d.Run(ctx, jobFor(dir), dir, func(e events.Event) { got = append(got, e) })
	if err != nil {
		t.Fatal(err)
	}
	if len(f.killed) == 0 || f.killed[0] != "SIGUSR1" {
		t.Fatalf("signals %v", f.killed)
	}
	// The box ended before its supervisor recorded the run: the worker records it, after the last
	// event the box sent.
	last := got[len(got)-1]
	if n, _ := last.Int("n"); last.Kind() != "run.end" || n != 2 || meta.Status != "failed" || !strings.Contains(meta.Note, "a line on stderr") {
		t.Fatalf("the worker's record: %v; meta %+v", last, meta)
	}
}

func TestABoxKilledForMemoryIsReportedSo(t *testing.T) {
	f := &fakeDocker{hold: make(chan struct{}), oom: true, exit: 137}
	close(f.hold)
	d := &Docker{Host: serveDocker(t, f), Image: "gongoozler:test"}
	dir := t.TempDir()
	meta, _, err := d.Run(context.Background(), jobFor(dir), dir, func(events.Event) {})
	if err != nil {
		t.Fatal(err)
	}
	if meta.Status != "oom" || meta.ExitCode != 137 || !strings.Contains(meta.Note, "memory") {
		t.Fatalf("meta %+v", meta)
	}
}

func TestTheDemuxerReadsFramesAcrossReads(t *testing.T) {
	var b bytes.Buffer
	for _, p := range []string{"hello ", "world\n"} {
		var hdr [8]byte
		hdr[0] = 1
		binary.BigEndian.PutUint32(hdr[4:], uint32(len(p)))
		b.Write(hdr[:])
		b.WriteString(p)
	}
	var got string
	if err := demux(io.MultiReader(bytes.NewReader(b.Bytes()[:5]), bytes.NewReader(b.Bytes()[5:])), func(s byte, p []byte) { got += string(p) }); err != nil {
		t.Fatal(err)
	}
	if got != "hello world\n" {
		t.Fatalf("got %q", got)
	}
	if err := demux(bytes.NewReader(b.Bytes()[:10]), func(byte, []byte) {}); err == nil {
		t.Fatal("a frame cut short is an error")
	}
}
