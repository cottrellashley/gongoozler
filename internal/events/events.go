// Package events is the CLI's side of the event protocol (spec/protocol.md, gongoozler-events/1).
//
// Producers (the Python library in a run, and the CLI itself) send JSON objects, one per line.
// An event stays a decoded JSON object, so kinds and fields this build does not know pass through
// unchanged. The CLI checks only the envelope: `v`, `kind`, `t` and `seq`.
package events

import (
	"bufio"
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"math"
	"os"
	"sync"
	"time"
)

// Protocol is the major version of gongoozler-events this build reads and writes.
const Protocol = 1

// Event is one decoded event.
type Event map[string]any

// Kind is the event's kind ("stage", "progress", ...).
func (e Event) Kind() string { s, _ := e["kind"].(string); return s }

// Str is a string field, or "".
func (e Event) Str(k string) string { s, _ := e[k].(string); return s }

// Num is a numeric field, and whether it is present and finite.
func (e Event) Num(k string) (float64, bool) {
	switch v := e[k].(type) {
	case float64:
		return v, !math.IsNaN(v) && !math.IsInf(v, 0)
	case json.Number:
		f, err := v.Float64()
		return f, err == nil
	case int:
		return float64(v), true
	case int64:
		return float64(v), true
	}
	return 0, false
}

// Int is an integral field (such as a nanosecond time), and whether it is present.
func (e Event) Int(k string) (int64, bool) {
	switch v := e[k].(type) {
	case json.Number:
		i, err := v.Int64()
		if err == nil {
			return i, true
		}
		f, err := v.Float64()
		return int64(f), err == nil
	case float64:
		return int64(v), true
	case int64:
		return v, true
	case int:
		return int64(v), true
	}
	return 0, false
}

// Bool is a boolean field, and whether it is present.
func (e Event) Bool(k string) (bool, bool) { b, ok := e[k].(bool); return b, ok }

// ErrEnvelope is wrapped by Decode's errors for lines that are JSON but not events.
var ErrEnvelope = errors.New("not a gongoozler event")

// Decode reads one line from a producer. Numbers stay exact (json.Number), so nanosecond times
// survive.
func Decode(line []byte) (Event, error) {
	d := json.NewDecoder(bytes.NewReader(line))
	d.UseNumber()
	var e Event
	if err := d.Decode(&e); err != nil {
		return nil, fmt.Errorf("not JSON: %w", err)
	}
	if _, err := d.Token(); err != io.EOF {
		return nil, fmt.Errorf("%w: data after the event", ErrEnvelope)
	}
	if e == nil {
		return nil, fmt.Errorf("%w: not an object", ErrEnvelope)
	}
	v, ok := e.Int("v")
	if !ok {
		return nil, fmt.Errorf("%w: no protocol version v", ErrEnvelope)
	}
	if v != Protocol {
		return nil, fmt.Errorf("%w: protocol %d, this build reads %d", ErrEnvelope, v, Protocol)
	}
	if e.Kind() == "" {
		return nil, fmt.Errorf("%w: no kind", ErrEnvelope)
	}
	if _, ok := e.Num("t"); !ok {
		return nil, fmt.Errorf("%w: no time t", ErrEnvelope)
	}
	if _, ok := e.Int("seq"); !ok {
		return nil, fmt.Errorf("%w: no sequence number seq", ErrEnvelope)
	}
	return e, nil
}

// New is an event the CLI makes itself, stamped with the protocol version and the time.
func New(kind string, fields map[string]any) Event {
	e := Event{"v": Protocol, "kind": kind, "t": float64(time.Now().UnixNano()) / 1e9, "src": "cli"}
	for k, v := range fields {
		e[k] = v
	}
	return e
}

// Log is a run's event log: events.jsonl, one event per line, each numbered `n` in the order the
// supervisor received it. Safe for concurrent use. The owner flushes it (on a ticker, and at once
// after events that matter); Close flushes and syncs it to disk.
type Log struct {
	mu sync.Mutex
	f  *os.File
	w  *bufio.Writer
	n  int64
}

// Create starts a new log at path, readable by its owner only.
func Create(path string) (*Log, error) {
	f, err := os.OpenFile(path, os.O_CREATE|os.O_WRONLY|os.O_EXCL, 0o600)
	if err != nil {
		return nil, err
	}
	return &Log{f: f, w: bufio.NewWriterSize(f, 1<<16)}, nil
}

// Append numbers e, writes it to the buffer, and returns it.
func (l *Log) Append(e Event) (Event, error) {
	l.mu.Lock()
	defer l.mu.Unlock()
	l.n++
	e["n"] = l.n
	b, err := json.Marshal(e)
	if err != nil {
		return e, err
	}
	_, err = l.w.Write(append(b, '\n'))
	return e, err
}

// Flush writes what is buffered to the file.
func (l *Log) Flush() error {
	l.mu.Lock()
	defer l.mu.Unlock()
	return l.w.Flush()
}

// Count is how many events have been appended.
func (l *Log) Count() int64 { l.mu.Lock(); defer l.mu.Unlock(); return l.n }

// Close flushes, syncs and closes the log.
func (l *Log) Close() error {
	l.mu.Lock()
	defer l.mu.Unlock()
	err := l.w.Flush()
	if serr := l.f.Sync(); err == nil {
		err = serr
	}
	if cerr := l.f.Close(); err == nil {
		err = cerr
	}
	return err
}

// Urgent kinds are flushed to the log file as soon as they are stored: what a reader of the file,
// or a crash, must not lose to a buffer.
func Urgent(kind string) bool {
	switch kind {
	case "run.start", "run.end", "stage", "error", "artifact", "hello", "cli.protocol":
		return true
	}
	return false
}

// Read decodes a stored log, calling fn for each event in order.
func Read(r io.Reader, fn func(Event) error) error {
	s := bufio.NewScanner(r)
	s.Buffer(make([]byte, 0, 1<<16), MaxLine)
	for line := 1; s.Scan(); line++ {
		d := json.NewDecoder(bytes.NewReader(s.Bytes()))
		d.UseNumber()
		var e Event
		if err := d.Decode(&e); err != nil {
			return fmt.Errorf("line %d: %w", line, err)
		}
		if err := fn(e); err != nil {
			return err
		}
	}
	return s.Err()
}

// MaxLine is the longest event line accepted, in bytes.
const MaxLine = 4 << 20
