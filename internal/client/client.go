// Package client speaks to a gongoozler server (spec/server.md), for the CLI and for workers.
package client

import (
	"bufio"
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"strconv"
	"strings"
	"time"

	"github.com/cottrellashley/gongoozler/internal/api"
	"github.com/cottrellashley/gongoozler/internal/events"
	"github.com/cottrellashley/gongoozler/internal/runs"
)

// Client is a server's address and the token for it.
type Client struct {
	Base  string
	Token string
	HTTP  *http.Client
}

// New is a client for the server at base (such as http://host:8150), with token if it is set.
func New(base, token string) *Client {
	return &Client{Base: strings.TrimRight(base, "/"), Token: token, HTTP: &http.Client{}}
}

// FromEnv is a client for $GONGOOZLER_SERVER with $GONGOOZLER_TOKEN, or nil when no server is set.
func FromEnv() *Client {
	if s := os.Getenv("GONGOOZLER_SERVER"); s != "" {
		return New(s, os.Getenv("GONGOOZLER_TOKEN"))
	}
	return nil
}

// ErrLeaseLost is what a worker's request for a job reads as once its lease is no longer held.
var ErrLeaseLost = errors.New("the lease is no longer held")

// Error is a server's refusal.
type Error struct {
	Code int
	Msg  string
}

func (e *Error) Error() string { return fmt.Sprintf("server: %s (HTTP %d)", e.Msg, e.Code) }

// IsNotFound reports whether err is the server saying it has no such thing.
func IsNotFound(err error) bool {
	var e *Error
	return errors.As(err, &e) && e.Code == http.StatusNotFound
}

func (c *Client) request(ctx context.Context, method, path string, body io.Reader, header map[string]string) (*http.Response, error) {
	req, err := http.NewRequestWithContext(ctx, method, c.Base+path, body)
	if err != nil {
		return nil, err
	}
	if c.Token != "" {
		req.Header.Set("Authorization", "Bearer "+c.Token)
	}
	for k, v := range header {
		req.Header.Set(k, v)
	}
	resp, err := c.HTTP.Do(req)
	if err != nil {
		return nil, err
	}
	if resp.StatusCode >= 400 {
		defer resp.Body.Close()
		var e struct {
			Error string `json:"error"`
		}
		b, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<16))
		if json.Unmarshal(b, &e) != nil || e.Error == "" {
			e.Error = strings.TrimSpace(string(b))
		}
		if resp.StatusCode == http.StatusConflict && header[api.LeaseHeader] != "" {
			return nil, ErrLeaseLost
		}
		return nil, &Error{resp.StatusCode, e.Error}
	}
	return resp, nil
}

func (c *Client) call(ctx context.Context, method, path string, in, out any, header map[string]string) (int, error) {
	var body io.Reader
	if in != nil {
		b, err := json.Marshal(in)
		if err != nil {
			return 0, err
		}
		body = bytes.NewReader(b)
		if header == nil {
			header = map[string]string{}
		}
		header["Content-Type"] = "application/json"
	}
	resp, err := c.request(ctx, method, path, body, header)
	if err != nil {
		return 0, err
	}
	defer resp.Body.Close()
	if out != nil && resp.StatusCode != http.StatusNoContent {
		d := json.NewDecoder(resp.Body)
		d.UseNumber()
		if err := d.Decode(out); err != nil {
			return resp.StatusCode, fmt.Errorf("server: an answer that is not the JSON expected: %w", err)
		}
	}
	return resp.StatusCode, nil
}

// Health is the server's version and how many runs it holds in each state.
func (c *Client) Health(ctx context.Context) (map[string]any, error) {
	var out map[string]any
	_, err := c.call(ctx, http.MethodGet, "/v1/health", nil, &out, nil)
	return out, err
}

// ------------------------------------------------------------------------------- blobs

// Upload stores the file at path as a blob, unless the server has it, and returns its SHA-256.
func (c *Client) Upload(ctx context.Context, path string) (string, int64, error) {
	f, err := os.Open(path)
	if err != nil {
		return "", 0, err
	}
	defer f.Close()
	h := sha256.New()
	n, err := io.Copy(h, f)
	if err != nil {
		return "", 0, err
	}
	sum := hex.EncodeToString(h.Sum(nil))
	if resp, err := c.request(ctx, http.MethodHead, "/v1/blobs/"+sum, nil, nil); err == nil {
		resp.Body.Close()
		return sum, n, nil
	}
	if _, err := f.Seek(0, io.SeekStart); err != nil {
		return "", 0, err
	}
	resp, err := c.request(ctx, http.MethodPut, "/v1/blobs/"+sum, f, map[string]string{"Content-Type": "application/octet-stream"})
	if err != nil {
		return "", 0, err
	}
	resp.Body.Close()
	return sum, n, nil
}

// Download writes blob sha to w.
func (c *Client) Download(ctx context.Context, sha string, w io.Writer) error {
	resp, err := c.request(ctx, http.MethodGet, "/v1/blobs/"+sha, nil, nil)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	_, err = io.Copy(w, resp.Body)
	return err
}

// ------------------------------------------------------------------------------- runs

// Submit queues a run.
func (c *Client) Submit(ctx context.Context, spec api.Spec) (runs.Meta, error) {
	var m runs.Meta
	_, err := c.call(ctx, http.MethodPost, "/v1/runs", spec, &m, nil)
	return m, err
}

// Run is a run's record.
func (c *Client) Run(ctx context.Context, id string) (runs.Meta, error) {
	var m runs.Meta
	_, err := c.call(ctx, http.MethodGet, "/v1/runs/"+url.PathEscape(id), nil, &m, nil)
	return m, err
}

// Runs is every run the server holds, oldest first.
func (c *Client) Runs(ctx context.Context) ([]runs.Meta, error) {
	var out []runs.Meta
	_, err := c.call(ctx, http.MethodGet, "/v1/runs", nil, &out, nil)
	return out, err
}

// Cancel asks for a run to stop: at once if it is queued, through its worker if it runs.
func (c *Client) Cancel(ctx context.Context, id string) (runs.Meta, error) {
	var m runs.Meta
	_, err := c.call(ctx, http.MethodPost, "/v1/runs/"+url.PathEscape(id)+"/cancel", nil, &m, nil)
	return m, err
}

// Events calls fn for each event stored now after number after, without following.
func (c *Client) Events(ctx context.Context, id string, after int64, fn func(events.Event) error) error {
	resp, err := c.request(ctx, http.MethodGet, fmt.Sprintf("/v1/runs/%s/events?follow=0&after=%d", url.PathEscape(id), after), nil, nil)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	return events.Read(resp.Body, fn)
}

// Follow calls fn for each event after number after, as the run makes them, until the run has
// ended (it returns its status) or ctx is done. A dropped connection is reopened where it broke off.
func (c *Client) Follow(ctx context.Context, id string, after int64, fn func(events.Event) error) (string, error) {
	backoff := 250 * time.Millisecond
	for {
		status, last, err := c.follow(ctx, id, after, fn)
		if status != "" || ctx.Err() != nil {
			return status, ctx.Err()
		}
		var se *Error
		if errors.As(err, &se) || errors.Is(err, errStop) {
			return "", err
		}
		if last > after {
			after, backoff = last, 250*time.Millisecond
		}
		select {
		case <-ctx.Done():
			return "", ctx.Err()
		case <-time.After(backoff):
		}
		backoff = min(backoff*2, 5*time.Second)
	}
}

var errStop = errors.New("stopped by the caller")

func (c *Client) follow(ctx context.Context, id string, after int64, fn func(events.Event) error) (string, int64, error) {
	resp, err := c.request(ctx, http.MethodGet, "/v1/runs/"+url.PathEscape(id)+"/events", nil,
		map[string]string{"Accept": "text/event-stream", "Last-Event-ID": strconv.FormatInt(after, 10)})
	if err != nil {
		return "", after, err
	}
	defer resp.Body.Close()
	sc := bufio.NewScanner(resp.Body)
	sc.Buffer(make([]byte, 0, 1<<16), events.MaxLine+64)
	var kind string
	var data []byte
	for sc.Scan() {
		line := sc.Bytes()
		switch {
		case len(line) == 0:
			if kind == "end" {
				var end struct {
					Status string `json:"status"`
				}
				if json.Unmarshal(data, &end) != nil || end.Status == "" {
					end.Status = "ended"
				}
				return end.Status, after, nil
			}
			if len(data) > 0 {
				d := json.NewDecoder(bytes.NewReader(data))
				d.UseNumber()
				var e events.Event
				if err := d.Decode(&e); err == nil {
					if n, ok := e.Int("n"); ok {
						after = n
					}
					if err := fn(e); err != nil {
						return "", after, fmt.Errorf("%w: %w", errStop, err)
					}
				}
			}
			kind, data = "", nil
		case bytes.HasPrefix(line, []byte(":")):
		case bytes.HasPrefix(line, []byte("event:")):
			kind = strings.TrimSpace(string(line[6:]))
		case bytes.HasPrefix(line, []byte("data:")):
			data = append(data, bytes.TrimPrefix(line[5:], []byte(" "))...)
		}
	}
	if err := sc.Err(); err != nil {
		return "", after, err
	}
	return "", after, io.ErrUnexpectedEOF
}

// Artifacts is what a run kept.
func (c *Client) Artifacts(ctx context.Context, id string) ([]api.Artifact, error) {
	var out []api.Artifact
	_, err := c.call(ctx, http.MethodGet, "/v1/runs/"+url.PathEscape(id)+"/artifacts", nil, &out, nil)
	return out, err
}

// Artifact writes one of a run's artifacts to w.
func (c *Client) Artifact(ctx context.Context, id, name string, w io.Writer) error {
	resp, err := c.request(ctx, http.MethodGet, "/v1/runs/"+url.PathEscape(id)+"/artifacts/"+url.PathEscape(name), nil, nil)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	_, err = io.Copy(w, resp.Body)
	return err
}

// ------------------------------------------------------------------------------- workers

// Lease asks for a job that fits offer, waiting up to offer.Wait; nil when there is none.
func (c *Client) Lease(ctx context.Context, offer api.Offer) (*api.Job, error) {
	var job api.Job
	code, err := c.call(ctx, http.MethodPost, "/v1/lease", offer, &job, nil)
	if err != nil || code == http.StatusNoContent {
		return nil, err
	}
	return &job, nil
}

func leaseHeader(lease string) map[string]string { return map[string]string{api.LeaseHeader: lease} }

// Heartbeat renews a lease, and says whether the run is to be cancelled.
func (c *Client) Heartbeat(ctx context.Context, id, lease string) (api.Beat, error) {
	var b api.Beat
	_, err := c.call(ctx, http.MethodPost, "/v1/runs/"+url.PathEscape(id)+"/heartbeat", map[string]any{}, &b, leaseHeader(lease))
	return b, err
}

// Push sends a run's events, in order, as its supervisor numbered them.
func (c *Client) Push(ctx context.Context, id, lease string, evs []events.Event) error {
	var buf bytes.Buffer
	for _, e := range evs {
		b, err := json.Marshal(e)
		if err != nil {
			continue
		}
		buf.Write(b)
		buf.WriteByte('\n')
	}
	h := leaseHeader(lease)
	h["Content-Type"] = "application/x-ndjson"
	resp, err := c.request(ctx, http.MethodPost, "/v1/runs/"+url.PathEscape(id)+"/events", &buf, h)
	if err != nil {
		return err
	}
	resp.Body.Close()
	return nil
}

// Complete reports a run's end, with the artifacts it kept (already uploaded as blobs).
func (c *Client) Complete(ctx context.Context, id, lease string, meta runs.Meta, artifacts []api.Artifact) error {
	if artifacts == nil {
		artifacts = []api.Artifact{}
	}
	_, err := c.call(ctx, http.MethodPost, "/v1/runs/"+url.PathEscape(id)+"/complete",
		map[string]any{"meta": meta, "artifacts": artifacts}, nil, leaseHeader(lease))
	return err
}

// Release gives a job back to the server, to run again elsewhere.
func (c *Client) Release(ctx context.Context, id, lease, reason string) error {
	_, err := c.call(ctx, http.MethodPost, "/v1/runs/"+url.PathEscape(id)+"/release", map[string]any{"reason": reason}, nil, leaseHeader(lease))
	return err
}
