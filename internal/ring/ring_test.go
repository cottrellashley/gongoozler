package ring

import (
	"encoding/json"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
)

// The Python writer is the one runs use, so these tests read rings it wrote.
func python(t *testing.T, dir, code string) {
	t.Helper()
	_, here, _, _ := runtime.Caller(0)
	src := filepath.Join(filepath.Dir(here), "..", "..", "python", "src")
	cmd := exec.Command("python3", "-c", code)
	cmd.Env = append(os.Environ(), "PYTHONPATH="+src, "GONGOOZLER_RING="+dir)
	if out, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("python: %v\n%s", err, out)
	}
}

func only(t *testing.T, dir string) *Ring {
	t.Helper()
	files, _ := filepath.Glob(filepath.Join(dir, "*.ring"))
	if len(files) != 1 {
		t.Fatalf("%d rings in %s", len(files), dir)
	}
	r, err := Open(files[0])
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(r.Close)
	return r
}

func drainAll(r *Ring) []map[string]any {
	var out []map[string]any
	r.Drain(func(p []byte) {
		var e map[string]any
		_ = json.Unmarshal(p, &e) // a record that is not JSON reads as an empty event, and fails the test's checks
		out = append(out, e)
	})
	return out
}

func TestRecordsAndSlotsWrittenByPythonReadBackExactly(t *testing.T) {
	dir := t.TempDir()
	python(t, dir, `
import gongoozler as gz
with gz.stage("load"):
    for i in range(1000):
        gz.progress("rows", i + 1, 1000, sim=1_700_000_000_123_456_789)
    gz.metric("nav", 101.5, at=7)
gz.log("done")
`)
	r := only(t, dir)
	evs := drainAll(r)
	var kinds []string
	for _, e := range evs {
		kinds = append(kinds, e["kind"].(string))
	}
	got := strings.Join(kinds, " ")
	// The slots updated before the stage ended are written ahead of its end.
	if got != "hello stage progress metric stage log" {
		t.Fatalf("records: %s", got)
	}
	if evs[2]["done"].(float64) != 1000 {
		t.Fatalf("the progress written ahead of the stage end is the latest: %v", evs[2])
	}
	slots := r.Slots(nil)
	if len(slots) != 2 {
		t.Fatalf("%d slots", len(slots))
	}
	p := slots[0]
	if p.Kind != Progress || p.Key != "rows" || p.A != 1000 || !p.AInt || p.B != 1000 || p.C != 1_700_000_000_123_456_789 || !p.HasC || p.HasD {
		t.Fatalf("progress slot %+v", p)
	}
	if m := slots[1]; m.Kind != Metric || m.Key != "nav" || m.A != 101.5 || m.C != 7 {
		t.Fatalf("metric slot %+v", m)
	}
	if c := r.Counters(); !c.Closed || c.Dropped != 0 {
		t.Fatalf("counters %+v", c)
	}
	if r.Pending() != 0 {
		t.Fatal("unread bytes after a full drain")
	}
	// A slot whose sequence has not moved is not reported again.
	if again := r.Slots([]uint64{slots[0].Seq, slots[1].Seq}); len(again) != 0 {
		t.Fatalf("unchanged slots reported: %v", again)
	}
}

func TestTheRingWrapsAndAFullRingDropsAndCounts(t *testing.T) {
	dir := t.TempDir()
	// 5,000 events of about 200 bytes do not fit 64 KiB unread: most are dropped, all counted.
	python(t, dir, `
import gongoozler._ring as ring, gongoozler._events as ev, gongoozler as gz, os
ev._capture.__wrapped__ if False else None
r = ring.Ring(os.environ["GONGOOZLER_RING"], data_size=1 << 16, slot_count=4)
for i in range(5000):
    r.write(('{"v":1,"kind":"x","t":1,"seq":%d,"pad":"%s"}' % (i + 1, "p" * 150)).encode())
r.close()
`)
	r := only(t, dir)
	first := drainAll(r)
	c := r.Counters()
	if len(first) == 0 || c.Dropped == 0 || uint64(len(first))+c.Dropped != 5000 {
		t.Fatalf("read %d, dropped %d", len(first), c.Dropped)
	}
	// After the reader has consumed, the writer has room again: 250 records (52 KB) fit, and they
	// cross the end of the 64 KiB data region, so they wrap.
	headBefore := *r.u64(oHead)
	python(t, dir, `
import gongoozler._ring as ring, glob, mmap, os, struct
path = glob.glob(os.path.join(os.environ["GONGOOZLER_RING"], "*.ring"))[0]
f = open(path, "r+b"); mm = mmap.mmap(f.fileno(), 0)
w = ring.Ring.__new__(ring.Ring)
w.mm, w.data_size = mm, 1 << 16
w.head = struct.unpack_from("<Q", mm, ring.O_HEAD)[0]
for i in range(250):
    assert w.write(('{"v":1,"kind":"y","t":1,"seq":%d,"pad":"%s"}' % (i + 1, "q" * 150)).encode()), i
`)
	second := drainAll(r)
	if len(second) != 250 || second[249]["seq"].(float64) != 250 {
		t.Fatalf("after wrapping: %d records", len(second))
	}
	if headBefore/(1<<16) == *r.u64(oHead)/(1<<16) {
		t.Fatal("the second batch never wrapped: the test does not test wrapping")
	}
}
