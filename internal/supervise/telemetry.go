package supervise

import (
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"time"

	"github.com/cottrellashley/gongoozler/internal/events"
	"github.com/cottrellashley/gongoozler/internal/ring"
)

// telemetry reads the rings the run's processes write (spec/ring.md), from outside them. Each poll
// reads each ring's new records, then its changed slots. Order follows the program's:
//   - a slot value made after a record is shown only once that record has been read (each value
//     carries the sequence number of the last record before it);
//   - a slot value made before a record is written into the ring ahead of it.
//
// Slots are shown at most ten times a second each.
type telemetry struct {
	dir   string
	o     Options
	pl    *pipeline
	runID string
	ps    map[string]*producer
	order []string
	stopc chan struct{}
	done  chan struct{}

	protocolErrors int64
	warnedVersion  bool
}

type producer struct {
	r       *ring.Ring
	last    []uint64
	emitted []time.Time
	units   map[string]string
	seq     int64
	stages  []string
}

func newTelemetry(dir string, o Options, pl *pipeline, runID string) *telemetry {
	return &telemetry{dir: dir, o: o, pl: pl, runID: runID, ps: map[string]*producer{}, stopc: make(chan struct{}), done: make(chan struct{})}
}

func (t *telemetry) loop() {
	defer close(t.done)
	tick := time.NewTicker(t.o.Poll)
	defer tick.Stop()
	for n := 0; ; n++ {
		select {
		case <-t.stopc:
			return
		case <-tick.C:
			if n%4 == 0 {
				t.scan()
			}
			t.pass(false)
		}
	}
}

func (t *telemetry) scan() {
	files, _ := filepath.Glob(filepath.Join(t.dir, "*.ring"))
	for _, f := range files {
		if _, ok := t.ps[f]; ok {
			continue
		}
		r, err := ring.Open(f)
		if err != nil {
			continue // not ready yet: the next scan
		}
		t.ps[f] = &producer{r: r, units: map[string]string{}}
		t.order = append(t.order, f)
	}
	sort.SliceStable(t.order, func(i, j int) bool { return t.ps[t.order[i]].r.Created < t.ps[t.order[j]].r.Created })
}

func (t *telemetry) pass(final bool) {
	for _, f := range t.order {
		p := t.ps[f]
		t.drain(p)
		t.sample(p, final)
	}
}

func (p *producer) grow(i int) {
	for len(p.last) <= i {
		p.last = append(p.last, 0)
		p.emitted = append(p.emitted, time.Time{})
	}
}

func (t *telemetry) sample(p *producer, final bool) {
	now := time.Now()
	for _, s := range p.r.Slots(p.last) {
		p.grow(s.Index)
		if s.After > uint64(p.seq) {
			continue // it follows a record not read yet: after the next drain
		}
		if !final && now.Sub(p.emitted[s.Index]) < 100*time.Millisecond {
			continue // read again next time: coalesced
		}
		p.last[s.Index], p.emitted[s.Index] = s.Seq, now
		e := events.Event{"v": events.Protocol, "seq": p.seq, "pid": p.r.PID}
		for k, v := range s.Event() {
			e[k] = v
		}
		if u := p.units[s.Key]; u != "" && s.Kind == ring.Progress {
			e["unit"] = u
		}
		if t.runID != "" {
			e["run"] = t.runID
		}
		t.pl.producer(e)
	}
}

func (t *telemetry) drain(p *producer) {
	p.r.Drain(func(payload []byte) {
		e, err := events.Decode(payload)
		if err != nil {
			t.protocolErrors++
			if t.protocolErrors <= 20 {
				t.pl.cli("cli.protocol", map[string]any{"error": err.Error(), "pid": p.r.PID})
			}
			return
		}
		if seq, ok := e.Int("seq"); ok && seq > p.seq {
			p.seq = seq
		}
		switch e.Kind() {
		case "progress.unit":
			p.units[e.Str("key")] = e.Str("unit")
			return
		case "progress", "metric":
			if slot, ok := e.Int("slot"); ok && slot >= 0 {
				sseq, _ := e.Int("sseq")
				p.grow(int(slot))
				if uint64(sseq) <= p.last[slot] {
					return // its slot was already read at this value or later
				}
				p.last[slot], p.emitted[slot] = uint64(sseq), time.Now()
			}
			if u := p.units[e.Str("key")]; u != "" && e.Kind() == "progress" {
				e["unit"] = u
			}
		case "hello":
			if lib := e.Str("lib"); lib != "" && t.o.Version != "dev" && lib != t.o.Version && !t.warnedVersion {
				t.warnedVersion = true
				t.pl.cli("cli.version", map[string]any{"error": fmt.Sprintf("the run's gongoozler library is %s, this supervisor is %s", lib, t.o.Version), "pid": p.r.PID})
			}
		case "stage":
			d, _ := e.Int("depth")
			depth := int(max(d, 0))
			if depth > len(p.stages) {
				depth = len(p.stages)
			}
			p.stages = p.stages[:depth]
			if e.Str("phase") == "begin" {
				p.stages = append(p.stages, e.Str("name"))
			}
		}
		if _, ok := e["pid"]; !ok {
			e["pid"] = p.r.PID
		}
		t.pl.producer(e)
	})
}

// Summary is what the run's telemetry lost or cut, and where it was when it ended.
type Summary struct {
	Producers                                       int
	Dropped, Truncated, Limited, SlotsFull, Unclean uint64
	CorruptBytes                                    uint64
	ProtocolErrors                                  int64
	OpenStages                                      []string
}

// Complete is true when nothing was dropped, cut, refused or unreadable. A process that ended
// without closing its ring (a crash) loses nothing by that alone: its ring outlives it.
func (s Summary) Complete() bool {
	return s.Dropped+s.Truncated+s.Limited+s.SlotsFull+s.CorruptBytes == 0 && s.ProtocolErrors == 0
}

func (s Summary) fields() map[string]any {
	return map[string]any{"complete": s.Complete(), "producers": s.Producers, "dropped": s.Dropped, "truncated": s.Truncated, "limited": s.Limited,
		"slots_full": s.SlotsFull, "unclean_exits": s.Unclean, "corrupt_bytes": s.CorruptBytes, "protocol_errors": s.ProtocolErrors}
}

func (s Summary) describe() string {
	var parts []string
	add := func(n uint64, what string) {
		if n > 0 {
			parts = append(parts, fmt.Sprintf("%d %s", n, what))
		}
	}
	add(s.Dropped, "events dropped (ring full)")
	add(s.Truncated, "events cut to fit")
	add(s.Limited, "events over a rate limit")
	add(s.SlotsFull, "updates with no slot left")
	add(s.CorruptBytes, "bytes unreadable")
	if s.ProtocolErrors > 0 {
		parts = append(parts, fmt.Sprintf("%d events not in the protocol", s.ProtocolErrors))
	}
	return strings.Join(parts, ", ")
}

// final stops the poller, reads everything left (rings made at the last moment included), and
// sums up. A ring whose writer is gone and whose next record never verifies is abandoned, and its
// bytes counted.
func (t *telemetry) final() Summary {
	close(t.stopc)
	<-t.done
	t.scan()
	for attempt := 0; attempt < 5; attempt++ {
		t.pass(true)
		pending := false
		for _, p := range t.ps {
			if p.r.Pending() > 0 {
				pending = true
			}
		}
		if !pending {
			break
		}
		time.Sleep(20 * time.Millisecond)
	}
	s := Summary{Producers: len(t.ps), ProtocolErrors: t.protocolErrors}
	for _, f := range t.order {
		p := t.ps[f]
		c := p.r.Counters()
		s.Dropped += c.Dropped
		s.Truncated += c.Truncated
		s.Limited += c.Limited
		s.SlotsFull += c.SlotsFull
		if !c.Closed {
			s.Unclean++
		}
		if p.r.Pending() > 0 {
			s.CorruptBytes += p.r.Skip()
		}
		if len(p.stages) > 0 && len(s.OpenStages) == 0 {
			s.OpenStages = append([]string(nil), p.stages...)
		}
		p.r.Close()
		os.Remove(f)
	}
	return s
}
