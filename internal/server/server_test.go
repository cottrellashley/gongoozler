package server

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/cottrellashley/gongoozler/internal/api"
	"github.com/cottrellashley/gongoozler/internal/client"
	"github.com/cottrellashley/gongoozler/internal/events"
	"github.com/cottrellashley/gongoozler/internal/runs"
)

func newServer(t *testing.T, cfg Config) (*Server, *client.Client, string) {
	t.Helper()
	if cfg.Dir == "" {
		cfg.Dir = t.TempDir()
	}
	s, err := New(cfg)
	if err != nil {
		t.Fatal(err)
	}
	hs := httptest.NewServer(s)
	t.Cleanup(func() { s.Close(); hs.Close() })
	return s, client.New(hs.URL, cfg.Token), hs.URL
}

func blob(t *testing.T, c *client.Client, content string) string {
	t.Helper()
	p := filepath.Join(t.TempDir(), "f")
	if err := os.WriteFile(p, []byte(content), 0o600); err != nil {
		t.Fatal(err)
	}
	sum, _, err := c.Upload(context.Background(), p)
	if err != nil {
		t.Fatal(err)
	}
	return sum
}

func submitOne(t *testing.T, c *client.Client, spec api.Spec) runs.Meta {
	t.Helper()
	if spec.SHA256 == "" {
		spec.SHA256 = blob(t, c, "print('hi')\n")
	}
	if spec.File == "" {
		spec.File = "idea.py"
	}
	m, err := c.Submit(context.Background(), spec)
	if err != nil {
		t.Fatal(err)
	}
	return m
}

func lease(t *testing.T, c *client.Client, o api.Offer) *api.Job {
	t.Helper()
	if o.Worker == "" {
		o.Worker = "w1"
	}
	job, err := c.Lease(context.Background(), o)
	if err != nil {
		t.Fatal(err)
	}
	return job
}

func must(t *testing.T, err error) {
	t.Helper()
	if err != nil {
		t.Fatal(err)
	}
}

func ev(kind string, n int64) events.Event {
	e := events.New(kind, map[string]any{"msg": kind})
	e["n"], e["seq"] = n, n
	return e
}

func stored(t *testing.T, c *client.Client, id string) []events.Event {
	t.Helper()
	var out []events.Event
	if err := c.Events(context.Background(), id, 0, func(e events.Event) error { out = append(out, e); return nil }); err != nil {
		t.Fatal(err)
	}
	return out
}

func kinds(evs []events.Event) string {
	var k []string
	for _, e := range evs {
		k = append(k, e.Kind())
	}
	return strings.Join(k, " ")
}

func TestAJobGoesToTheFirstWorkerItFitsAndRunsToItsEnd(t *testing.T) {
	_, c, _ := newServer(t, Config{})
	big := submitOne(t, c, api.Spec{Memory: 64 << 30})
	small := submitOne(t, c, api.Spec{Memory: 1 << 30, Params: map[string]any{"x": 1}})
	if job := lease(t, c, api.Offer{Memory: 8 << 30, CPUs: 4}); job == nil || job.ID != small.ID {
		t.Fatalf("an 8 GiB worker got %+v, want the 1 GiB job", job)
	}
	if job := lease(t, c, api.Offer{Memory: 8 << 30, CPUs: 4, Wait: 0.05}); job != nil {
		t.Fatalf("the 64 GiB job went to an 8 GiB worker")
	}
	job := lease(t, c, api.Offer{Memory: 128 << 30, CPUs: 4})
	if job == nil || job.ID != big.ID || job.Attempt != 1 {
		t.Fatalf("got %+v", job)
	}
	ctx := context.Background()
	if err := c.Push(ctx, job.ID, job.Lease, []events.Event{ev("run.start", 1), ev("log", 2)}); err != nil {
		t.Fatal(err)
	}
	// A push retried after a timeout is stored once.
	if err := c.Push(ctx, job.ID, job.Lease, []events.Event{ev("log", 2), ev("run.end", 3)}); err != nil {
		t.Fatal(err)
	}
	art := blob(t, c, `{"score": 1}`)
	if err := c.Complete(ctx, job.ID, job.Lease, runs.Meta{Status: "ok", Host: "box-1", Wall: 1.5}, []api.Artifact{{Name: "stats.json", SHA256: art, Bytes: 12}}); err != nil {
		t.Fatal(err)
	}
	if got := kinds(stored(t, c, job.ID)); got != "job.queued job.leased run.start log run.end" {
		t.Fatalf("stored %s", got)
	}
	m, _ := c.Run(ctx, job.ID)
	if m.Status != "ok" || m.Host != "box-1" || m.Worker != "w1" || m.Attempt != 1 || m.Events != 5 {
		t.Fatalf("meta %+v", m)
	}
	var buf bytes.Buffer
	if err := c.Artifact(ctx, job.ID, "stats.json", &buf); err != nil || buf.String() != `{"score": 1}` {
		t.Fatalf("artifact: %v %q", err, buf.String())
	}
	// Once the job has ended its lease is no use.
	if err := c.Push(ctx, job.ID, job.Lease, []events.Event{ev("log", 4)}); err != client.ErrLeaseLost {
		t.Fatalf("a push after the end: %v", err)
	}
}

func TestALeaseThatExpiresRunsTheJobAgainUntilItsAttemptsAreSpent(t *testing.T) {
	_, c, _ := newServer(t, Config{TTL: 100 * time.Millisecond, MaxAttempts: 2})
	m := submitOne(t, c, api.Spec{})
	first := lease(t, c, api.Offer{})
	must(t, c.Push(context.Background(), first.ID, first.Lease, []events.Event{ev("run.start", 1)}))
	second := lease(t, c, api.Offer{Worker: "w2", Wait: 2})
	if second == nil || second.ID != m.ID || second.Attempt != 2 || second.Lease == first.Lease {
		t.Fatalf("the expired job was not leased again: %+v", second)
	}
	// The first worker, back from a pause, holds nothing.
	if _, err := c.Heartbeat(context.Background(), first.ID, first.Lease); err != client.ErrLeaseLost {
		t.Fatalf("the old lease: %v", err)
	}
	// The second attempt's events are numbered by its own supervisor, from 1 again.
	must(t, c.Push(context.Background(), second.ID, second.Lease, []events.Event{ev("run.start", 1)}))
	deadline := time.Now().Add(5 * time.Second)
	for {
		got, _ := c.Run(context.Background(), m.ID)
		if got.Status == "lost" {
			if !strings.Contains(got.Note, "on each of 2 attempts") {
				t.Fatalf("note %q", got.Note)
			}
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("still %s", got.Status)
		}
		time.Sleep(20 * time.Millisecond)
	}
	evs := stored(t, c, m.ID)
	want := "job.queued job.leased run.start job.expired job.requeued job.leased run.start job.expired run.end"
	if got := kinds(evs); got != want {
		t.Fatalf("stored\n %s\nwant\n %s", got, want)
	}
	if a, _ := evs[6].Int("attempt"); a != 2 {
		t.Fatalf("the second attempt's events say attempt %v", evs[6]["attempt"])
	}
}

func TestHeartbeatsKeepALeaseAndCarryACancellation(t *testing.T) {
	_, c, _ := newServer(t, Config{TTL: 150 * time.Millisecond})
	m := submitOne(t, c, api.Spec{})
	job := lease(t, c, api.Offer{})
	for i := 0; i < 6; i++ {
		time.Sleep(50 * time.Millisecond)
		beat, err := c.Heartbeat(context.Background(), job.ID, job.Lease)
		if err != nil || beat.Cancel {
			t.Fatalf("beat %d: %+v %v", i, beat, err)
		}
	}
	if _, err := c.Cancel(context.Background(), m.ID); err != nil {
		t.Fatal(err)
	}
	beat, err := c.Heartbeat(context.Background(), job.ID, job.Lease)
	if err != nil || !beat.Cancel {
		t.Fatalf("after cancelling: %+v %v", beat, err)
	}
	// A job cancelled before any worker took it ends at once.
	q := submitOne(t, c, api.Spec{Memory: 1 << 50})
	got, _ := c.Cancel(context.Background(), q.ID)
	if got.Status != "cancelled" || !strings.HasSuffix(kinds(stored(t, c, q.ID)), "run.end") {
		t.Fatalf("cancelled while queued: %+v", got)
	}
}

func TestAFollowerResumesWhereItBrokeOffAndStopsAtTheEnd(t *testing.T) {
	_, c, _ := newServer(t, Config{})
	m := submitOne(t, c, api.Spec{})
	job := lease(t, c, api.Offer{})
	ctx := context.Background()
	must(t, c.Push(ctx, job.ID, job.Lease, []events.Event{ev("run.start", 1), ev("log", 2)}))
	var seen []int64
	done := make(chan string)
	go func() {
		status, _ := c.Follow(ctx, m.ID, 2, func(e events.Event) error {
			n, _ := e.Int("n")
			seen = append(seen, n)
			return nil
		})
		done <- status
	}()
	time.Sleep(50 * time.Millisecond)
	must(t, c.Push(ctx, job.ID, job.Lease, []events.Event{ev("log", 3), ev("run.end", 4)}))
	must(t, c.Complete(ctx, job.ID, job.Lease, runs.Meta{Status: "ok"}, nil))
	select {
	case status := <-done:
		if status != "ok" {
			t.Fatalf("ended with %q", status)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("the follower did not stop at the run's end")
	}
	if len(seen) != 4 || seen[0] != 3 || seen[3] != 6 {
		t.Fatalf("followed from event 2: got %v", seen)
	}
}

func TestBlobsAreCheckedAgainstTheirNames(t *testing.T) {
	_, _, url := newServer(t, Config{})
	wrong := strings.Repeat("0", 64)
	req, _ := http.NewRequest(http.MethodPut, url+"/v1/blobs/"+wrong, strings.NewReader("content"))
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	resp.Body.Close()
	if resp.StatusCode != http.StatusBadRequest {
		t.Fatalf("a blob under the wrong name: %d", resp.StatusCode)
	}
	sum := sha256.Sum256([]byte("content"))
	req, _ = http.NewRequest(http.MethodPut, url+"/v1/blobs/"+hex.EncodeToString(sum[:]), strings.NewReader("content"))
	resp, _ = http.DefaultClient.Do(req)
	resp.Body.Close()
	if resp.StatusCode != http.StatusCreated {
		t.Fatalf("a blob under its name: %d", resp.StatusCode)
	}
	for _, bad := range []string{"../etc", "ABC"} {
		resp, _ := http.Get(url + "/v1/blobs/" + bad)
		resp.Body.Close()
		if resp.StatusCode != http.StatusNotFound {
			t.Fatalf("%s: %d", bad, resp.StatusCode)
		}
	}
	// A run cannot name a blob the server does not have.
	c := client.New(url, "")
	if _, err := c.Submit(context.Background(), api.Spec{File: "x.py", SHA256: wrong}); err == nil || !strings.Contains(err.Error(), "upload the file") {
		t.Fatalf("submitting a missing blob: %v", err)
	}
}

func TestATokenIsRequiredWhenOneIsSet(t *testing.T) {
	_, c, url := newServer(t, Config{Token: "secret"})
	if _, err := c.Health(context.Background()); err != nil {
		t.Fatalf("with the token: %v", err)
	}
	if _, err := client.New(url, "wrong").Health(context.Background()); err == nil || !strings.Contains(err.Error(), "401") {
		t.Fatalf("with a wrong token: %v", err)
	}
	if _, err := client.New(url, "").Runs(context.Background()); err == nil {
		t.Fatal("with no token")
	}
}

func TestAServerRestartedPutsItsUnfinishedJobsBackInTheQueue(t *testing.T) {
	dir := t.TempDir()
	s, c, _ := newServer(t, Config{Dir: dir})
	m := submitOne(t, c, api.Spec{})
	lease(t, c, api.Offer{})
	s.Close()
	_, c2, _ := newServer(t, Config{Dir: dir})
	job := lease(t, c2, api.Offer{Worker: "w2"})
	if job == nil || job.ID != m.ID || job.Attempt != 2 {
		t.Fatalf("after a restart: %+v", job)
	}
	if got := kinds(stored(t, c2, m.ID)); got != "job.queued job.leased job.requeued job.leased" {
		t.Fatalf("stored %s", got)
	}
}
