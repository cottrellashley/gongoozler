package ring

import (
	"encoding/binary"
	"hash/crc32"
	"os"
	"path/filepath"
	"testing"
	"time"
	"unsafe"
)

// build is a well-formed ring: dataSize bytes of records, then slotCount slots, the records
// written from tail 0.
func build(dataSize uint64, slotCount uint32, records ...[]byte) []byte {
	le := binary.LittleEndian
	mem := make([]byte, Header+dataSize+uint64(slotCount)*slotSize)
	copy(mem[oMagic:], Magic[:])
	le.PutUint32(mem[oVersion:], 1)
	le.PutUint64(mem[oDataSize:], dataSize)
	le.PutUint64(mem[oSlotsOff:], Header+dataSize)
	le.PutUint32(mem[oSlotCount:], slotCount)
	le.PutUint32(mem[oSlotSize:], slotSize)
	var head uint64
	for _, p := range records {
		pos := Header + head%dataSize
		le.PutUint32(mem[pos:], uint32(len(p)))
		le.PutUint32(mem[pos+4:], crc32.ChecksumIEEE(p))
		copy(mem[pos+8:], p)
		head += align(8 + uint64(len(p)))
	}
	le.PutUint64(mem[oHead:], head)
	if slotCount > 0 {
		off := Header + dataSize
		key := []byte("steps")
		mem[off+12] = Progress
		le.PutUint16(mem[off+14:], uint16(len(key)))
		copy(mem[off+slotKeyOff:], key)
		le.PutUint32(mem[off+8:], crc32.ChecksumIEEE(mem[off+slotBodyOff:off+slotKeyOff+uint64(len(key))]))
		le.PutUint64(mem[off:], 2)
		le.PutUint64(mem[oSlotsUsed:], 1)
	}
	return mem
}

func TestAHeadFarPastTheTailCannotHoldADrainForever(t *testing.T) {
	mem := build(64, 0)
	for i := Header; i < Header+64; i++ {
		mem[i] = 0xFF // every position a wrap marker
	}
	binary.LittleEndian.PutUint64(mem[oHead:], 1<<62)
	path := filepath.Join(t.TempDir(), "r.ring")
	if err := os.WriteFile(path, mem, 0o600); err != nil {
		t.Fatal(err)
	}
	r, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer r.Close()
	if n := r.Drain(func([]byte) { t.Fatal("a record from nothing") }); n != 0 {
		t.Fatalf("read %d", n)
	}
}

func TestCountersNearTheirLimitCannotWrapADrainRound(t *testing.T) {
	mem := build(64, 0)
	for i := Header; i < Header+64; i++ {
		mem[i] = 0xFF
	}
	le := binary.LittleEndian
	le.PutUint64(mem[oTail:], ^uint64(0)-8)
	le.PutUint64(mem[oHead:], ^uint64(0))
	path := filepath.Join(t.TempDir(), "r.ring")
	if err := os.WriteFile(path, mem, 0o600); err != nil {
		t.Fatal(err)
	}
	r, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer r.Close()
	done := make(chan int)
	go func() { done <- r.Drain(func([]byte) {}) }()
	select {
	case n := <-done:
		if n != 0 {
			t.Fatalf("read %d", n)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("the drain wrapped round")
	}
}

func TestAWellFormedRingBuiltHereReadsBack(t *testing.T) {
	path := filepath.Join(t.TempDir(), "r.ring")
	if err := os.WriteFile(path, build(1024, 2, []byte(`{"a":1}`), []byte(`{"b":2}`)), 0o600); err != nil {
		t.Fatal(err)
	}
	r, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer r.Close()
	var got []string
	r.Drain(func(p []byte) { got = append(got, string(p)) })
	if len(got) != 2 || got[1] != `{"b":2}` {
		t.Fatalf("got %q", got)
	}
	if s := r.Slots(nil); len(s) != 1 || s[0].Key != "steps" {
		t.Fatalf("slots %+v", s)
	}
}

// FuzzRing reads rings whose header counters and contents are anything at all: it must never
// panic, never hang, and never hand on a record that does not verify.
func FuzzRing(f *testing.F) {
	f.Add(uint8(4), uint8(2), uint64(48), uint64(0), uint64(1), []byte(`{"kind":"log"}`))
	f.Add(uint8(0), uint8(1), uint64(1<<62), uint64(8), uint64(9), []byte{0xFF, 0xFF, 0xFF, 0xFF})
	f.Add(uint8(9), uint8(0), uint64(0), uint64(1<<40), uint64(0), []byte{})
	dir := f.TempDir()
	f.Fuzz(func(t *testing.T, exp, slots uint8, head, tail, used uint64, data []byte) {
		dataSize := uint64(64) << (exp % 10)
		mem := build(dataSize, uint32(slots%8), []byte(`{"kind":"seed"}`))
		copy(mem[Header:], data)
		le := binary.LittleEndian
		le.PutUint64(mem[oHead:], head)
		le.PutUint64(mem[oTail:], tail)
		le.PutUint64(mem[oSlotsUsed:], used)
		path := filepath.Join(dir, "fuzz.ring")
		if err := os.WriteFile(path, mem, 0o600); err != nil {
			t.Fatal(err)
		}
		r, err := Open(path)
		if err != nil {
			t.Fatalf("a well-formed header refused: %v", err)
		}
		defer r.Close()
		for i := 0; i < 3; i++ {
			r.Drain(func(p []byte) {
				if uint64(len(p)) > dataSize {
					t.Fatalf("a record of %d bytes from a ring of %d", len(p), dataSize)
				}
			})
			r.Slots(nil)
			r.Counters()
			r.Pending()
		}
		r.Skip()
	})
}

// FuzzOpen reads headers of any bytes: a malformed one is refused, never a panic.
func FuzzOpen(f *testing.F) {
	f.Add(build(64, 1)[:Header+64+slotSize])
	f.Add(build(64, 0)[:Header])
	f.Fuzz(func(t *testing.T, in []byte) {
		if len(in) > 64<<10 {
			return // FuzzRing covers big rings; this is for headers
		}
		words := make([]uint64, (len(in)+7)/8) // as a mapping is: 8-byte aligned
		mem := unsafe.Slice((*byte)(unsafe.Pointer(unsafe.SliceData(words))), len(words)*8)[:len(in)]
		copy(mem, in)
		if r, err := parse(mem); err == nil {
			r.Drain(func([]byte) {})
			r.Slots(nil)
		}
	})
}
