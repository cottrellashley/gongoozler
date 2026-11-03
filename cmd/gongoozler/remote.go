package main

import (
	"bufio"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"os/signal"
	"path/filepath"
	"syscall"
	"time"

	"github.com/cottrellashley/gongoozler/internal/api"
	"github.com/cottrellashley/gongoozler/internal/client"
	"github.com/cottrellashley/gongoozler/internal/events"
	"github.com/cottrellashley/gongoozler/internal/runs"
	"github.com/cottrellashley/gongoozler/internal/term"
)

// submit uploads a planned file (and its lock) and queues it on the server. The run is recorded
// here too, as a remote run, so ls, events, diff and rerun treat it like any other.
func submit(ctx context.Context, p *plan, args []string, params map[string]any, state, sweepID string) (runs.Meta, error) {
	c := p.client
	sum, _, err := c.Upload(ctx, p.file)
	if err != nil {
		return runs.Meta{}, fmt.Errorf("cannot upload %s: %w", filepath.Base(p.file), err)
	}
	var lock string
	if _, err := os.Stat(p.file + ".lock"); err == nil {
		if lock, _, err = c.Upload(ctx, p.file+".lock"); err != nil {
			return runs.Meta{}, fmt.Errorf("cannot upload its lock: %w", err)
		}
	}
	if args == nil {
		args = []string{}
	}
	spec := api.Spec{File: filepath.Base(p.file), SHA256: sum, Lock: lock, Args: args, Params: params,
		Memory: p.settings.Memory, CPUs: p.settings.CPUs, Timeout: p.settings.Timeout.Seconds(), Sweep: sweepID}
	m, err := c.Submit(ctx, spec)
	if err != nil {
		return runs.Meta{}, err
	}
	m.File, m.Placement, m.Server = p.file, "remote", c.Base
	if cwd, err := os.Getwd(); err == nil {
		m.Cwd = cwd
	}
	dir := runs.Dir(state, m.ID)
	for _, d := range []string{"artifacts", "source"} {
		if err := os.MkdirAll(filepath.Join(dir, d), 0o700); err != nil {
			return m, err
		}
	}
	// The copies let `gongoozler rerun` run the file as it was; the run goes on without them.
	_ = copyInto(p.file, filepath.Join(dir, "source"))
	if lock != "" {
		_ = copyInto(p.file+".lock", filepath.Join(dir, "source"))
	}
	return m, runs.WriteMeta(state, m)
}

// copyInto copies a file into dir, under its own name.
func copyInto(src, dir string) error {
	b, err := os.ReadFile(src)
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(dir, filepath.Base(src)), b, 0o600)
}

func remoteRun(p *plan, args []string, params map[string]any, m term.Mode, detach bool, state string, stdout, stderr io.Writer, sweepID string) int {
	meta, err := submit(context.Background(), p, args, params, state, sweepID)
	if err != nil {
		return failf(stderr, 1, "%v", err)
	}
	if detach {
		fmt.Fprintln(stdout, meta.ID)
		fmt.Fprintf(stderr, "submitted %s to %s: gongoozler attach %s\n", meta.ID, p.client.Base, meta.ID)
		return 0
	}
	return watchRemote(p.client, state, meta.ID, m, true, stdout, stderr)
}

// watchRemote follows a remote run, showing it and mirroring its events here, until it ends; then
// it fetches its record and artifacts. When own is true the run is this command's: the first
// interrupt cancels it on the server, and a second stops watching. Otherwise an interrupt only
// stops watching.
func watchRemote(c *client.Client, state, id string, m term.Mode, own bool, stdout, stderr io.Writer) int {
	ctx, stop := context.WithCancel(context.Background())
	defer stop()
	sigs := make(chan os.Signal, 2)
	signal.Notify(sigs, syscall.SIGINT, syscall.SIGTERM, syscall.SIGHUP)
	defer signal.Stop(sigs)
	go func() {
		n := 0
		for range sigs {
			n++
			if own && n == 1 {
				fmt.Fprintf(stderr, "gongoozler: cancelling %s on the server (interrupt again to stop watching; it would go on)\n", id)
				if _, err := c.Cancel(context.Background(), id); err != nil {
					fmt.Fprintf(stderr, "gongoozler: could not cancel %s: %v\n", id, err)
				}
				continue
			}
			stop()
			return
		}
	}()
	r := renderer(m, stdout, stderr)
	mirror, after, err := openMirror(state, id)
	if err != nil {
		r.Close()
		return failf(stderr, 1, "%v", err)
	}
	status, err := c.Follow(ctx, id, after, func(e events.Event) error {
		mirror.write(e)
		show(r, m, e)
		return nil
	})
	mirror.close()
	r.Close()
	if status == "" {
		if ctx.Err() != nil {
			fmt.Fprintf(stderr, "gongoozler: stopped watching %s; it goes on (gongoozler attach %s)\n", id, id)
			return 0
		}
		return failf(stderr, 1, "lost touch with the server: %v (gongoozler attach %s)", err, id)
	}
	meta, err := pullRun(context.Background(), c, state, id, false)
	if err != nil {
		return failf(stderr, 1, "the run ended (%s), and its record could not be fetched: %v", status, err)
	}
	if m != term.JSON {
		fmt.Fprintf(stderr, "events: %s\n", filepath.Join(runs.Dir(state, id), "events.jsonl"))
	}
	return meta.ExitCode
}

// mirror is a remote run's event log kept here, as the server numbered it.
type mirror struct {
	f *os.File
	w *bufio.Writer
}

func openMirror(state, id string) (*mirror, int64, error) {
	path := filepath.Join(runs.Dir(state, id), "events.jsonl")
	var after int64
	if f, err := os.Open(path); err == nil {
		// A line cut short at the end stops the read; the mirror goes on after the last whole one.
		_ = events.Read(f, func(e events.Event) error {
			if n, ok := e.Int("n"); ok {
				after = n
			}
			return nil
		})
		f.Close()
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		return nil, 0, err
	}
	f, err := os.OpenFile(path, os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0o600)
	if err != nil {
		return nil, 0, err
	}
	return &mirror{f: f, w: bufio.NewWriterSize(f, 1<<16)}, after, nil
}

func (mi *mirror) write(e events.Event) {
	if b, err := json.Marshal(e); err == nil {
		mi.w.Write(append(b, '\n'))
	}
	if events.Urgent(e.Kind()) {
		mi.w.Flush()
	}
}

func (mi *mirror) close() {
	mi.w.Flush()
	mi.f.Close()
}

// pullRun brings a remote run's record up to date here, with the events stored since the last pull
// and, once it has ended, its artifacts. With all, it fetches every event again.
func pullRun(ctx context.Context, c *client.Client, state, id string, all bool) (runs.Meta, error) {
	remote, err := c.Run(ctx, id)
	if err != nil {
		return runs.Meta{}, err
	}
	local, _ := runs.ReadMeta(state, id)
	dir := runs.Dir(state, id)
	if err := os.MkdirAll(filepath.Join(dir, "artifacts"), 0o700); err != nil {
		return runs.Meta{}, err
	}
	if all {
		os.Remove(filepath.Join(dir, "events.jsonl"))
	}
	mi, after, err := openMirror(state, id)
	if err != nil {
		return runs.Meta{}, err
	}
	err = c.Events(ctx, id, after, func(e events.Event) error { mi.write(e); return nil })
	mi.close()
	if err != nil {
		return runs.Meta{}, err
	}
	if remote.Status != "queued" && remote.Status != "running" {
		arts, err := c.Artifacts(ctx, id)
		if err != nil {
			return runs.Meta{}, err
		}
		for _, a := range arts {
			if err := fetchArtifact(ctx, c, id, a, filepath.Join(dir, "artifacts")); err != nil {
				return runs.Meta{}, fmt.Errorf("artifact %s: %w", a.Name, err)
			}
		}
	}
	m := remote
	if local.File != "" {
		m.File, m.Cwd = local.File, local.Cwd
	}
	m.Placement, m.Server = "remote", c.Base
	return m, runs.WriteMeta(state, m)
}

func fetchArtifact(ctx context.Context, c *client.Client, id string, a api.Artifact, dir string) error {
	dst := filepath.Join(dir, filepath.Base(a.Name))
	if sum, err := sha256File(dst); err == nil && sum == a.SHA256 {
		return nil
	}
	tmp := dst + ".partial"
	f, err := os.OpenFile(tmp, os.O_CREATE|os.O_WRONLY|os.O_TRUNC, 0o600)
	if err != nil {
		return err
	}
	err = c.Artifact(ctx, id, a.Name, f)
	if cerr := f.Close(); err == nil {
		err = cerr
	}
	if err == nil {
		if sum, _ := sha256File(tmp); sum != a.SHA256 {
			err = fmt.Errorf("its content does not match its SHA-256")
		}
	}
	if err != nil {
		os.Remove(tmp)
		return err
	}
	return os.Rename(tmp, dst)
}

func pullCmd(args []string, stdout, stderr io.Writer) int {
	fs := flags("pull", "RUN", stderr)
	if err := fs.Parse(args); err != nil {
		return 2
	}
	if fs.NArg() != 1 {
		fs.Usage()
		return 2
	}
	state, ok := stateDir(stderr)
	if !ok {
		return 1
	}
	m, err := resolve(state, fs.Arg(0))
	if err != nil {
		return failf(stderr, 1, "%v", err)
	}
	if m.Placement != "remote" {
		return failf(stderr, 1, "run %s ran here: everything about it is already in %s", m.ID, runs.Dir(state, m.ID))
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Minute)
	defer cancel()
	m, err = pullRun(ctx, clientFor(m), state, m.ID, true)
	if err != nil {
		return failf(stderr, 1, "%v", err)
	}
	fmt.Fprintf(stdout, "%s %s: %s\n", m.ID, m.Status, runs.Dir(state, m.ID))
	return 0
}

// clientFor is a client for the server a remote run went to.
func clientFor(m runs.Meta) *client.Client {
	if m.Server != "" {
		return client.New(m.Server, os.Getenv("GONGOOZLER_TOKEN"))
	}
	return client.FromEnv()
}
