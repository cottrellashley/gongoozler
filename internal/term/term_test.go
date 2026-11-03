package term

import (
	"bytes"
	"encoding/json"
	"regexp"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/cottrellashley/gongoozler/internal/events"
)

func ev(t *testing.T, s string) events.Event {
	t.Helper()
	d := json.NewDecoder(strings.NewReader(s))
	d.UseNumber()
	var e events.Event
	if err := d.Decode(&e); err != nil {
		t.Fatal(err)
	}
	return e
}

var stamp = regexp.MustCompile(`^\[ *[0-9.]+s\] `)

func TestPlainModePrintsOneLinePerChangeWorthReading(t *testing.T) {
	var out, status bytes.Buffer
	r := New(Plain, &out, &status, nil)
	r.Event(ev(t, `{"kind":"run.start","id":"R1","file":"/x/ema.py"}`))
	r.Event(ev(t, `{"kind":"stage","name":"simulation","phase":"begin","depth":0}`))
	for _, done := range []int{10, 12, 14, 80, 100} { // 12 and 14 move too little to print
		r.Event(ev(t, `{"kind":"progress","key":"simulation","done":`+strconv.Itoa(done)+`,"total":100,"sim":1546383600000000000,"rate":1500000}`))
	}
	r.Event(ev(t, `{"kind":"stage","name":"simulation","phase":"end","depth":0,"elapsed":2.5,"ok":true}`))
	r.Output("stdout", "hello from the run")
	r.Event(ev(t, `{"kind":"run.end","id":"R1","status":"ok","wall":3.2,"peak":1610612736}`))
	r.Close()
	var lines []string
	for _, l := range strings.Split(strings.TrimSpace(status.String()), "\n") {
		lines = append(lines, stamp.ReplaceAllString(l, ""))
	}
	want := []string{
		"run R1: /x/ema.py",
		"▸ simulation",
		"simulation   10.0%  2019-01-01T23:00:00Z  1.5M/s",
		"simulation   80.0%  2019-01-01T23:00:00Z  1.5M/s",
		"simulation  100.0%  2019-01-01T23:00:00Z  1.5M/s",
		"✓ simulation (2.5s)",
		"run R1 ok in 3.2s, memory peak 1.5 GiB",
	}
	if strings.Join(lines, "\n") != strings.Join(want, "\n") {
		t.Fatalf("got:\n%s\nwant:\n%s", strings.Join(lines, "\n"), strings.Join(want, "\n"))
	}
	if !strings.Contains(out.String(), "hello from the run") {
		t.Fatal("the run's stdout went missing")
	}
}

func TestJSONModePrintsEveryEventAndNothingElse(t *testing.T) {
	var out, status bytes.Buffer
	r := New(JSON, &out, &status, nil)
	r.Event(ev(t, `{"kind":"stage","name":"s","phase":"begin","depth":0,"n":1}`))
	r.Output("stdout", "carried as an event instead")
	r.Close()
	if strings.TrimSpace(out.String()) != `{"depth":0,"kind":"stage","n":1,"name":"s","phase":"begin"}` || status.Len() != 0 {
		t.Fatalf("stdout %q stderr %q", out.String(), status.String())
	}
}

func TestLiveModeRedrawsItsBlockInPlace(t *testing.T) {
	var out, status bytes.Buffer
	r := New(Live, &out, &status, func() int { return 40 })
	r.Event(ev(t, `{"kind":"stage","name":"simulation","phase":"begin","depth":0}`))
	r.Output("stderr", "a warning")
	r.Close()
	s := status.String()
	if !strings.Contains(s, "\x1b[1A\x1b[2K") {
		t.Fatal("the block was never erased before a line was printed above it")
	}
	if !strings.Contains(s, "a warning\n") {
		t.Fatal("the run's stderr went missing")
	}
}

func TestFormatting(t *testing.T) {
	cases := map[string]string{
		Bytes(512):              "512 B",
		Bytes(1536):             "1.5 KiB",
		Bytes(3.5 * (1 << 30)):  "3.5 GiB",
		Duration(0.42):          "0.4s",
		Duration(245):           "4m05s",
		Duration(7380):          "2h03m",
		Number(950):             "950",
		Number(12_345):          "12.3k",
		Number(4_500_000):       "4.5M",
		Number(0.1234567):       "0.1235",
		truncate("abcdefgh", 5): "abcd…",
	}
	for got, want := range cases {
		if got != want {
			t.Errorf("got %q, want %q", got, want)
		}
	}
}

// stalled is a terminal that stops reading until it is let go.
type stalled struct {
	mu  sync.Mutex
	let chan struct{}
	buf bytes.Buffer
}

func (s *stalled) Write(p []byte) (int, error) {
	<-s.let
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.buf.Write(p)
}

func (s *stalled) String() string { s.mu.Lock(); defer s.mu.Unlock(); return s.buf.String() }

func TestATerminalThatStopsReadingNeverHoldsTheRun(t *testing.T) {
	for _, mode := range []Mode{Live, Plain} {
		term := &stalled{let: make(chan struct{})}
		r := New(mode, term, term, nil)
		t0 := time.Now()
		for i := 0; i < 200_000; i++ {
			r.Output("stdout", "a line the run printed")
			if i%1000 == 0 {
				r.Event(events.Event{"kind": "progress", "key": "k", "done": float64(i), "total": 2e5})
			}
		}
		if took := time.Since(t0); took > time.Second {
			t.Fatalf("mode %d: queueing 200,000 lines took %v with the terminal stalled", mode, took)
		}
		close(term.let)
		r.Close()
		if !strings.Contains(term.String(), "lines not shown: the terminal could not keep up") {
			t.Fatalf("mode %d: the lines it could not show were not counted", mode)
		}
	}
}
