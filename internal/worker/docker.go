package worker

import (
	"archive/tar"
	"bufio"
	"bytes"
	"context"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"os"
	"path"
	"path/filepath"
	"strings"
	"sync"
	"time"

	"github.com/cottrellashley/gongoozler/internal/api"
	"github.com/cottrellashley/gongoozler/internal/events"
	"github.com/cottrellashley/gongoozler/internal/runs"
)

// Docker runs each job in a container of its own (spec/server.md, "Boxes"), through the Docker
// Engine API. The container runs the same supervisor as a local run (`gongoozler run --json`),
// which accounts and limits the run by the container's cgroup; its events come back on the
// container's stdout. The job's files go in, and its record and artifacts come out, through the
// API's archives, so the worker needs no filesystem shared with the containers.
type Docker struct {
	// Host is the Docker daemon: unix:///var/run/docker.sock (the default, or $DOCKER_HOST), or
	// tcp://host:port.
	Host string
	// Image has gongoozler, uv and Python, with /job and /state writable (see the Dockerfile).
	Image string
	// Network is the containers' network mode; empty for Docker's default.
	Network string
	Env     []string
	// Grace is how long a cancelled run gets before its container is killed. Default 10 s.
	Grace time.Duration

	once sync.Once
	http *http.Client
	base string
	api  string // the API version prefix, as negotiated: /v1.41, say
}

// dockerAPI is the API version used when the daemon does not say which it speaks. The daemon's
// own version is used when it says (on /_ping): daemons refuse versions older than their minimum.
const dockerAPI = "/v1.41"

func (d *Docker) init() {
	d.once.Do(func() {
		host := d.Host
		if host == "" {
			host = os.Getenv("DOCKER_HOST")
		}
		if host == "" {
			host = "unix:///var/run/docker.sock"
		}
		if sock, ok := strings.CutPrefix(host, "unix://"); ok {
			d.base = "http://docker"
			d.http = &http.Client{Transport: &http.Transport{DialContext: func(ctx context.Context, _, _ string) (net.Conn, error) {
				return (&net.Dialer{}).DialContext(ctx, "unix", sock)
			}}}
		} else {
			d.base = "http://" + strings.TrimPrefix(host, "tcp://")
			d.http = &http.Client{}
		}
		if d.Grace == 0 {
			d.Grace = 10 * time.Second
		}
		d.api = d.negotiate()
	})
}

// negotiate asks the daemon which API version it speaks (the Api-Version header of /_ping, as
// Docker's own client does), and uses that.
func (d *Docker) negotiate() string {
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, d.base+"/_ping", nil)
	if err != nil {
		return dockerAPI
	}
	resp, err := d.http.Do(req)
	if err != nil {
		return dockerAPI
	}
	defer resp.Body.Close()
	_, _ = io.Copy(io.Discard, resp.Body)
	if v := resp.Header.Get("Api-Version"); v != "" && !strings.ContainsAny(v, "/?#") {
		return "/v" + v
	}
	return dockerAPI
}

// dockerError is the daemon's refusal.
type dockerError struct {
	code int
	msg  string
}

func (e *dockerError) Error() string { return fmt.Sprintf("docker: %s (HTTP %d)", e.msg, e.code) }

func (d *Docker) do(ctx context.Context, method, p string, q url.Values, body io.Reader, ctype string) (*http.Response, error) {
	u := d.base + d.api + p
	if len(q) > 0 {
		u += "?" + q.Encode()
	}
	req, err := http.NewRequestWithContext(ctx, method, u, body)
	if err != nil {
		return nil, err
	}
	if ctype != "" {
		req.Header.Set("Content-Type", ctype)
	}
	resp, err := d.http.Do(req)
	if err != nil {
		return nil, err
	}
	if resp.StatusCode >= 400 {
		defer resp.Body.Close()
		var e struct {
			Message string `json:"message"`
		}
		b, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<16))
		if json.Unmarshal(b, &e) != nil || e.Message == "" {
			e.Message = strings.TrimSpace(string(b))
		}
		return nil, &dockerError{resp.StatusCode, e.Message}
	}
	return resp, nil
}

func (d *Docker) json(ctx context.Context, method, p string, q url.Values, in, out any) error {
	var body io.Reader
	ctype := ""
	if in != nil {
		b, err := json.Marshal(in)
		if err != nil {
			return err
		}
		body, ctype = bytes.NewReader(b), "application/json"
	}
	resp, err := d.do(ctx, method, p, q, body, ctype)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	if out == nil {
		_, _ = io.Copy(io.Discard, resp.Body) // read to the end, so the connection is reused
		return nil
	}
	return json.NewDecoder(resp.Body).Decode(out)
}

// ContainerSpec is what a job's container is created with: the part of the Engine API's create
// request a box needs.
type ContainerSpec struct {
	Image      string            `json:"Image"`
	Entrypoint []string          `json:"Entrypoint"`
	Cmd        []string          `json:"Cmd"`
	Env        []string          `json:"Env"`
	WorkingDir string            `json:"WorkingDir"`
	Labels     map[string]string `json:"Labels"`
	HostConfig HostConfig        `json:"HostConfig"`
}

// HostConfig is the container's limits: memory with no swap, CPUs, an init to reap and forward.
type HostConfig struct {
	Memory      int64  `json:"Memory,omitempty"`
	MemorySwap  int64  `json:"MemorySwap,omitempty"`
	NanoCPUs    int64  `json:"NanoCpus,omitempty"`
	Init        bool   `json:"Init"`
	NetworkMode string `json:"NetworkMode,omitempty"`
}

// Spec is the container a job runs in.
func (d *Docker) Spec(job api.Job) ContainerSpec {
	params, _ := json.Marshal(job.Spec.Params)
	file := filepath.Base(job.Spec.File)
	// The entrypoint is set, not left to the image, so the command is the same whatever the image's.
	cmd := []string{"run", "--json", "--local", "--no-lock", "--id", job.ID, "--params-json", string(params)}
	if job.Spec.Timeout > 0 {
		cmd = append(cmd, "--timeout", (time.Duration(job.Spec.Timeout * float64(time.Second))).String())
	}
	cmd = append(append(cmd, "/job/"+file), job.Spec.Args...)
	s := ContainerSpec{
		Image: d.Image, Entrypoint: []string{"gongoozler"}, Cmd: cmd, WorkingDir: "/job",
		Env:        append([]string{"GONGOOZLER_STATE=/state", "PYTHONUNBUFFERED=1"}, d.Env...),
		Labels:     map[string]string{"gongoozler.run": job.ID, "gongoozler.attempt": fmt.Sprint(job.Attempt)},
		HostConfig: HostConfig{Init: true, NetworkMode: d.Network},
	}
	if job.Spec.Memory > 0 {
		s.HostConfig.Memory, s.HostConfig.MemorySwap = job.Spec.Memory, job.Spec.Memory
	}
	if job.Spec.CPUs > 0 {
		s.HostConfig.NanoCPUs = int64(job.Spec.CPUs * 1e9)
	}
	return s
}

// Run runs a job in a new container, and removes the container once its record and artifacts are
// out of it.
func (d *Docker) Run(ctx context.Context, job api.Job, dir string, emit func(events.Event)) (runs.Meta, []string, error) {
	d.init()
	bg := context.Background()
	var created struct {
		ID string `json:"Id"`
	}
	name := fmt.Sprintf("gongoozler-%s-%d", job.ID, job.Attempt)
	if err := d.json(bg, http.MethodPost, "/containers/create", url.Values{"name": {name}}, d.Spec(job), &created); err != nil {
		return runs.Meta{}, nil, err
	}
	cid := created.ID
	defer func() {
		// Once its record and artifacts are out, the container goes; one that will not is Docker's
		// to show (labelled gongoozler.run).
		_ = d.json(bg, http.MethodDelete, "/containers/"+cid, url.Values{"force": {"1"}, "v": {"1"}}, nil, nil)
	}()
	in, err := tarFiles(dir, filepath.Base(job.Spec.File), filepath.Base(job.Spec.File)+".lock")
	if err != nil {
		return runs.Meta{}, nil, err
	}
	resp, err := d.do(bg, http.MethodPut, "/containers/"+cid+"/archive", url.Values{"path": {"/job"}}, bytes.NewReader(in), "application/x-tar")
	if err != nil {
		return runs.Meta{}, nil, fmt.Errorf("cannot put the job's files in its container: %w", err)
	}
	resp.Body.Close()
	if err := d.json(bg, http.MethodPost, "/containers/"+cid+"/start", nil, nil, nil); err != nil {
		return runs.Meta{}, nil, err
	}
	stopped := make(chan struct{})
	go func() {
		select {
		case <-stopped:
		case <-ctx.Done():
			// The supervisor inside takes SIGUSR1 as a cancellation, as for `gongoozler kill`.
			// A container that has just ended cannot be signalled: no matter.
			_ = d.json(bg, http.MethodPost, "/containers/"+cid+"/kill", url.Values{"signal": {"SIGUSR1"}}, nil, nil)
			select {
			case <-stopped:
			case <-time.After(d.Grace):
				_ = d.json(bg, http.MethodPost, "/containers/"+cid+"/kill", url.Values{"signal": {"SIGKILL"}}, nil, nil)
			}
		}
	}()
	last, stderr, logErr := d.follow(bg, cid, emit)
	var waited struct {
		StatusCode int `json:"StatusCode"`
	}
	waitErr := d.json(bg, http.MethodPost, "/containers/"+cid+"/wait", nil, nil, &waited)
	close(stopped)
	var inspect struct {
		State struct {
			OOMKilled bool `json:"OOMKilled"`
		} `json:"State"`
	}
	_ = d.json(bg, http.MethodGet, "/containers/"+cid+"/json", nil, nil, &inspect) // only to tell an out-of-memory kill

	out := filepath.Join(dir, "out")
	meta, arts, err := d.collect(bg, cid, job.ID, out)
	if err == nil {
		return meta, arts, nil
	}
	// The supervisor inside did not record the run's end: the container itself was killed (for
	// memory, most likely), or the supervisor could not start. The worker records it instead.
	status, note := "failed", "its box ended before its supervisor recorded the run"
	switch {
	case inspect.State.OOMKilled:
		status, note = "oom", "the kernel killed its box for memory: the box's memory limit is the job's"
	case logErr != nil:
		note += ": " + logErr.Error()
	case waitErr != nil:
		note += ": " + waitErr.Error()
	}
	if s := strings.TrimSpace(stderr); s != "" {
		note += "; it said: " + s
	}
	code := waited.StatusCode
	if code == 0 {
		code = 1
	}
	host, _ := os.Hostname()
	m := runs.Meta{ID: job.ID, File: job.Spec.File, SHA256: job.Spec.SHA256, Status: status, ExitCode: code, Note: note, Host: host, Placement: "box"}
	end := events.New("run.end", map[string]any{"id": job.ID, "status": status, "exit_code": code, "note": note, "wall": 0.0, "peak": 0})
	end["src"], end["n"] = "worker", last+1
	emit(end)
	return m, nil, nil
}

// follow passes on the events the container prints on stdout until it ends, and returns the last
// event number seen and what it printed on stderr (its last 4 KiB).
func (d *Docker) follow(ctx context.Context, cid string, emit func(events.Event)) (int64, string, error) {
	resp, err := d.do(ctx, http.MethodGet, "/containers/"+cid+"/logs", url.Values{"follow": {"1"}, "stdout": {"1"}, "stderr": {"1"}}, nil, "")
	if err != nil {
		return 0, "", err
	}
	defer resp.Body.Close()
	var last int64
	var stdout []byte
	var stderr tailBuffer
	err = demux(resp.Body, func(stream byte, p []byte) {
		if stream != 1 {
			stderr.Write(p)
			return
		}
		stdout = append(stdout, p...)
		for {
			i := bytes.IndexByte(stdout, '\n')
			if i < 0 {
				break
			}
			if e := stored(stdout[:i]); e != nil {
				if n, ok := e.Int("n"); ok {
					last = n
				}
				emit(e)
			}
			stdout = stdout[i+1:]
		}
	})
	return last, stderr.String(), err
}

// stored reads one event as the box's supervisor stored and printed it: numbered `n`. Its own
// events (run.start, output, artifact, run.end) carry no producer's `seq`, so this is not the
// producer's envelope check (events.Decode), only that it is an object with a kind and a number.
func stored(line []byte) events.Event {
	d := json.NewDecoder(bytes.NewReader(line))
	d.UseNumber()
	var e events.Event
	if d.Decode(&e) != nil || e == nil || e.Kind() == "" {
		return nil
	}
	if _, ok := e.Int("n"); !ok {
		return nil
	}
	return e
}

// demux reads the Engine API's multiplexed stream: frames of an 8-byte header (the stream, then
// the length, big-endian) and the payload.
func demux(r io.Reader, fn func(stream byte, p []byte)) error {
	br := bufio.NewReaderSize(r, 1<<16)
	var hdr [8]byte
	for {
		if _, err := io.ReadFull(br, hdr[:]); err != nil {
			if errors.Is(err, io.EOF) {
				return nil
			}
			return err
		}
		n := binary.BigEndian.Uint32(hdr[4:])
		p := make([]byte, n)
		if _, err := io.ReadFull(br, p); err != nil {
			return err
		}
		fn(hdr[0], p)
	}
}

type tailBuffer struct{ b []byte }

func (t *tailBuffer) Write(p []byte) {
	t.b = append(t.b, p...)
	if len(t.b) > 4096 {
		t.b = t.b[len(t.b)-4096:]
	}
}

func (t *tailBuffer) String() string { return string(t.b) }

// collect copies the run's record and artifacts out of the container into out.
func (d *Docker) collect(ctx context.Context, cid, id, out string) (runs.Meta, []string, error) {
	resp, err := d.do(ctx, http.MethodGet, "/containers/"+cid+"/archive", url.Values{"path": {"/state/runs/" + id}}, nil, "")
	if err != nil {
		return runs.Meta{}, nil, err
	}
	defer resp.Body.Close()
	if err := os.MkdirAll(filepath.Join(out, "artifacts"), 0o700); err != nil {
		return runs.Meta{}, nil, err
	}
	var meta runs.Meta
	var found bool
	var arts []string
	tr := tar.NewReader(resp.Body)
	for {
		h, err := tr.Next()
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			return runs.Meta{}, nil, err
		}
		if h.Typeflag != tar.TypeReg {
			continue
		}
		// Entries are named from the archived directory: ID/meta.json, ID/artifacts/NAME.
		parts := strings.Split(path.Clean(h.Name), "/")
		switch {
		case len(parts) == 2 && parts[1] == "meta.json":
			d := json.NewDecoder(io.LimitReader(tr, 16<<20))
			d.UseNumber()
			if err := d.Decode(&meta); err != nil {
				return runs.Meta{}, nil, err
			}
			found = true
		case len(parts) == 3 && parts[1] == "artifacts" && parts[2] != "" && !strings.HasSuffix(parts[2], ".partial"):
			dst := filepath.Join(out, "artifacts", filepath.Base(parts[2]))
			f, err := os.OpenFile(dst, os.O_CREATE|os.O_WRONLY|os.O_TRUNC, 0o600)
			if err != nil {
				return runs.Meta{}, nil, err
			}
			_, err = io.Copy(f, tr)
			if cerr := f.Close(); err == nil {
				err = cerr
			}
			if err != nil {
				return runs.Meta{}, nil, err
			}
			arts = append(arts, dst)
		}
	}
	if !found || meta.Status == "" || meta.Status == "running" {
		return runs.Meta{}, nil, errors.New("the run's record is not in its box")
	}
	return meta, arts, nil
}

// tarFiles is a tar of the named files in dir (those that exist).
func tarFiles(dir string, names ...string) ([]byte, error) {
	var buf bytes.Buffer
	tw := tar.NewWriter(&buf)
	for _, name := range names {
		b, err := os.ReadFile(filepath.Join(dir, name))
		if errors.Is(err, os.ErrNotExist) {
			continue
		}
		if err != nil {
			return nil, err
		}
		if err := tw.WriteHeader(&tar.Header{Name: name, Mode: 0o644, Size: int64(len(b)), ModTime: time.Now(), Typeflag: tar.TypeReg}); err != nil {
			return nil, err
		}
		if _, err := tw.Write(b); err != nil {
			return nil, err
		}
	}
	if err := tw.Close(); err != nil {
		return nil, err
	}
	return buf.Bytes(), nil
}
