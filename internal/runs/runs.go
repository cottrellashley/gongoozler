// Package runs is where runs live on disk, and what is recorded about each.
//
//	STATE/runs/ID/meta.json      written when the run starts (status "running"), and when it ends
//	STATE/runs/ID/events.jsonl   every event, numbered in the order received
//	STATE/runs/ID/output.log     everything the run printed
//	STATE/runs/ID/artifacts/     the files the run declared, copied and hashed
//	STATE/runs/ID/source/        the file and its lock, as run
//
// STATE is $GONGOOZLER_STATE, else $XDG_STATE_HOME/gongoozler, else ~/.local/state/gongoozler.
// Everything under it is the owner's alone (directories 0700, files 0600).
package runs

import (
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"sort"
	"time"

	"github.com/cottrellashley/gongoozler/internal/procmem"
)

// Meta is what a run's meta.json records.
type Meta struct {
	ID       string         `json:"id"`
	File     string         `json:"file"`
	SHA256   string         `json:"sha256"`
	Lock     string         `json:"lock_sha256,omitempty"`
	Args     []string       `json:"args"`
	Params   map[string]any `json:"params,omitempty"`
	Started  time.Time      `json:"started"`
	Ended    time.Time      `json:"ended,omitzero"`
	Status   string         `json:"status"`
	ExitCode int            `json:"exit_code"`
	Signal   string         `json:"signal,omitempty"`
	Note     string         `json:"note,omitempty"`
	Wall     float64        `json:"wall"`
	Peak     int64          `json:"peak"`
	Events   int64          `json:"events"`

	MemorySource string         `json:"memory_source,omitempty"`
	OOMKills     int64          `json:"oom_kills,omitempty"`
	Leftovers    int            `json:"leftovers,omitempty"`
	Telemetry    map[string]any `json:"telemetry,omitempty"`

	Host            string `json:"host"`
	Cwd             string `json:"cwd,omitempty"`
	SupervisorPID   int    `json:"supervisor_pid"`
	SupervisorStart uint64 `json:"supervisor_start"`
	RunPID          int    `json:"run_pid,omitempty"`
	RunStart        uint64 `json:"run_start,omitempty"`
	Version         string `json:"version,omitempty"`
	Placement       string `json:"placement,omitempty"`
	Server          string `json:"server,omitempty"`
	Worker          string `json:"worker,omitempty"`
	Attempt         int    `json:"attempt,omitempty"`
	Sweep           string `json:"sweep,omitempty"`
}

// StateDir resolves STATE.
func StateDir() (string, error) {
	if d := os.Getenv("GONGOOZLER_STATE"); d != "" {
		return d, nil
	}
	if d := os.Getenv("XDG_STATE_HOME"); d != "" {
		return filepath.Join(d, "gongoozler"), nil
	}
	home, err := os.UserHomeDir()
	if err != nil {
		return "", err
	}
	return filepath.Join(home, ".local", "state", "gongoozler"), nil
}

// NewID is a run ID that sorts by start time: 20261103T120000Z-1a2b3c4d.
func NewID(now time.Time) string {
	b := make([]byte, 4)
	rand.Read(b)
	return now.UTC().Format("20060102T150405Z") + "-" + hex.EncodeToString(b)
}

// Dir is a run's directory.
func Dir(state, id string) string { return filepath.Join(state, "runs", id) }

// WriteMeta records a run, atomically.
func WriteMeta(state string, m Meta) error {
	b, err := json.MarshalIndent(m, "", "  ")
	if err != nil {
		return err
	}
	dir := Dir(state, m.ID)
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return err
	}
	tmp := filepath.Join(dir, "meta.json.tmp")
	if err := os.WriteFile(tmp, append(b, '\n'), 0o600); err != nil {
		return err
	}
	return os.Rename(tmp, filepath.Join(dir, "meta.json"))
}

// ReadMeta reads one run's record.
func ReadMeta(state, id string) (Meta, error) {
	var m Meta
	b, err := os.ReadFile(filepath.Join(Dir(state, id), "meta.json"))
	if err != nil {
		return m, err
	}
	return m, json.Unmarshal(b, &m)
}

// Live reports how a run marked running really is: "running" while its supervisor lives (the
// same process, by PID and start time, on this host), "lost" once it is gone, and "remote" for a
// run supervised elsewhere.
func Live(m Meta) string {
	if m.Status != "running" {
		return m.Status
	}
	if m.Placement == "remote" {
		return "running"
	}
	host, _ := os.Hostname()
	if m.Host != "" && m.Host != host {
		return "running"
	}
	if procmem.Alive("/proc", m.SupervisorPID, m.SupervisorStart) {
		return "running"
	}
	return "lost"
}

// List is every run, oldest first, with "lost" in place of "running" for a run whose supervisor
// is gone.
func List(state string) ([]Meta, error) {
	entries, err := os.ReadDir(filepath.Join(state, "runs"))
	if errors.Is(err, os.ErrNotExist) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	var out []Meta
	for _, d := range entries {
		if !d.IsDir() {
			continue
		}
		m, err := ReadMeta(state, d.Name())
		if err != nil {
			continue
		}
		m.Status = Live(m)
		out = append(out, m)
	}
	sort.Slice(out, func(i, j int) bool { return out[i].ID < out[j].ID })
	return out, nil
}
