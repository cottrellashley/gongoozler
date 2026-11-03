// Package api is the server's wire format (spec/server.md): what a client submits, what a worker
// leases, and what each sends back. Everything is JSON over HTTP; a run's events stream as
// server-sent events.
package api

import "regexp"

// Spec is a run to submit. The file and its lock travel as blobs, named by their SHA-256.
type Spec struct {
	File    string         `json:"file"`
	SHA256  string         `json:"sha256"`
	Lock    string         `json:"lock_sha256,omitempty"`
	Args    []string       `json:"args"`
	Params  map[string]any `json:"params"`
	Memory  int64          `json:"memory,omitempty"`
	CPUs    float64        `json:"cpus,omitempty"`
	Timeout float64        `json:"timeout,omitempty"` // seconds
	Sweep   string         `json:"sweep,omitempty"`
}

// Offer is what a worker can take on when it asks for a job.
type Offer struct {
	Worker string  `json:"worker"`
	Memory int64   `json:"memory"` // free bytes; 0 takes only jobs that declare none
	CPUs   float64 `json:"cpus"`   // free CPUs
	Wait   float64 `json:"wait"`   // seconds to wait for a job before answering none
}

// Job is a run leased to a worker. The lease lasts TTL seconds past the last heartbeat or push.
type Job struct {
	ID      string  `json:"id"`
	Lease   string  `json:"lease"`
	Attempt int     `json:"attempt"`
	TTL     float64 `json:"ttl"`
	Spec    Spec    `json:"spec"`
}

// Beat is the server's answer to a heartbeat.
type Beat struct {
	Cancel bool `json:"cancel"`
}

// Artifact is a file a run kept, stored as a blob.
type Artifact struct {
	Name   string `json:"name"`
	SHA256 string `json:"sha256"`
	Bytes  int64  `json:"bytes"`
}

// LeaseHeader carries a worker's lease on the requests it makes for a job.
const LeaseHeader = "X-Gongoozler-Lease"

var sha = regexp.MustCompile(`^[0-9a-f]{64}$`)

// ValidSHA256 reports whether s names a blob: 64 lowercase hex digits.
func ValidSHA256(s string) bool { return sha.MatchString(s) }

var id = regexp.MustCompile(`^[0-9A-Za-z][0-9A-Za-z._-]{0,127}$`)

// ValidID reports whether s can name a run (and so a directory).
func ValidID(s string) bool { return id.MatchString(s) && s != "." && s != ".." }
