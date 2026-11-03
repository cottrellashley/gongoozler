package events

import (
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestDecodeChecksTheEnvelopeAndKeepsNanosecondsExact(t *testing.T) {
	e, err := Decode([]byte(`{"v":1,"kind":"progress","t":1.5,"seq":3,"sim":1793729490125645678,"extra":{"a":1}}`))
	if err != nil {
		t.Fatal(err)
	}
	if sim, _ := e.Int("sim"); sim != 1793729490125645678 {
		t.Fatalf("sim %d: a nanosecond time lost precision", sim)
	}
	if e.Kind() != "progress" {
		t.Fatal(e.Kind())
	}
	for line, want := range map[string]string{
		`[1,2]`:                            "not JSON",
		`{"kind":"x","t":1,"seq":1}`:       "no protocol version",
		`{"v":2,"kind":"x","t":1,"seq":1}`: "protocol 2, this build reads 1",
		`{"v":1,"t":1,"seq":1}`:            "no kind",
		`{"v":1,"kind":"x","seq":1}`:       "no time",
		`{"v":1,"kind":"x","t":1}`:         "no sequence number",
		`not json`:                         "not JSON",
	} {
		_, err := Decode([]byte(line))
		if err == nil || !strings.Contains(err.Error(), want) {
			t.Errorf("%s: got %v, want %q", line, err, want)
		}
		if want != "not JSON" && !errors.Is(err, ErrEnvelope) {
			t.Errorf("%s: not an envelope error", line)
		}
	}
}

func TestTheLogNumbersEventsInOrderAndReadsBack(t *testing.T) {
	path := filepath.Join(t.TempDir(), "events.jsonl")
	l, err := Create(path)
	if err != nil {
		t.Fatal(err)
	}
	for i := range 3 {
		if _, err := l.Append(New("note", map[string]any{"i": i})); err != nil {
			t.Fatal(err)
		}
	}
	if err := l.Close(); err != nil {
		t.Fatal(err)
	}
	f, _ := os.Open(path)
	defer f.Close()
	var got []int64
	if err := Read(f, func(e Event) error { n, _ := e.Int("n"); got = append(got, n); return nil }); err != nil {
		t.Fatal(err)
	}
	if len(got) != 3 || got[0] != 1 || got[2] != 3 {
		t.Fatalf("numbers %v", got)
	}
	if _, err := Create(path); err == nil {
		t.Fatal("a log was overwritten")
	}
}
