// Package ring reads the shared-memory rings runs write their events into (spec/ring.md).
//
// A ring is a file a run's process maps and writes; the supervisor maps it too and reads it from
// outside the process, so what was written survives the process. Records are checked by their
// crc32 before they are consumed: a record whose bytes are not all visible yet (on a CPU that
// reorders stores) fails the check and is read again on the next poll. Slots are read under their
// sequence lock and checked by crc32 the same way.
package ring

import (
	"encoding/binary"
	"errors"
	"fmt"
	"hash/crc32"
	"math"
	"os"
	"sync/atomic"
	"syscall"
	"unsafe"
)

// The header's fields, as byte offsets (spec/ring.md).
const (
	Header      = 4096
	skip        = 0xFFFFFFFF
	slotSize    = 256
	oMagic      = 0
	oVersion    = 8
	oDataSize   = 16
	oSlotsOff   = 24
	oSlotCount  = 32
	oSlotSize   = 36
	oPID        = 40
	oCreated    = 48
	oHead       = 64
	oTail       = 128
	oDropped    = 192
	oTruncated  = 200
	oLimited    = 208
	oClosed     = 216
	oSlotsUsed  = 224
	oSlotsFull  = 232
	maxRecord   = 64 << 20
	slotKeyOff  = 80
	slotBodyOff = 12
)

// Magic opens every ring, written last by its writer: a reader seeing it sees a whole header.
var Magic = [8]byte{'G', 'Z', 'R', 'I', 'N', 'G', '0', '1'}

// ErrNotReady means the file exists but its writer has not finished its header.
var ErrNotReady = errors.New("ring not ready")

// Ring is one mapped ring.
type Ring struct {
	Path      string
	PID       int
	Created   int64
	mem       []byte
	mapped    bool
	dataSize  uint64
	slotsOff  uint64
	slotCount uint64
}

// Open maps the ring at path.
func Open(path string) (*Ring, error) {
	f, err := os.OpenFile(path, os.O_RDWR, 0)
	if err != nil {
		return nil, err
	}
	defer f.Close()
	st, err := f.Stat()
	if err != nil {
		return nil, err
	}
	if st.Size() < Header {
		return nil, ErrNotReady
	}
	mem, err := syscall.Mmap(int(f.Fd()), 0, int(st.Size()), syscall.PROT_READ|syscall.PROT_WRITE, syscall.MAP_SHARED)
	if err != nil {
		return nil, fmt.Errorf("mapping %s: %w", path, err)
	}
	r, err := parse(mem)
	if err != nil {
		syscall.Munmap(mem)
		if errors.Is(err, ErrNotReady) {
			return nil, err
		}
		return nil, fmt.Errorf("%s: %w", path, err)
	}
	r.Path, r.mapped = path, true
	return r, nil
}

// parse checks a ring's header (mem is 8-byte aligned, as a mapping is) and reads its layout.
func parse(mem []byte) (*Ring, error) {
	if len(mem) < Header || [8]byte(mem[oMagic:oMagic+8]) != Magic {
		return nil, ErrNotReady
	}
	le := binary.LittleEndian
	r := &Ring{
		mem:       mem,
		dataSize:  le.Uint64(mem[oDataSize:]),
		slotsOff:  le.Uint64(mem[oSlotsOff:]),
		slotCount: uint64(le.Uint32(mem[oSlotCount:])),
		PID:       int(le.Uint32(mem[oPID:])),
		Created:   int64(le.Uint64(mem[oCreated:])),
	}
	if v := le.Uint32(mem[oVersion:]); v != 1 {
		return nil, fmt.Errorf("ring version %d, this build reads 1", v)
	}
	if le.Uint32(mem[oSlotSize:]) != slotSize || r.dataSize < 64 || r.dataSize&(r.dataSize-1) != 0 || r.slotsOff != Header+r.dataSize ||
		r.slotsOff+r.slotCount*slotSize > uint64(len(mem)) {
		return nil, errors.New("malformed ring header")
	}
	return r, nil
}

// Close unmaps the ring.
func (r *Ring) Close() {
	if r.mem != nil && r.mapped {
		syscall.Munmap(r.mem)
	}
	r.mem = nil
}

func (r *Ring) u64(off int) *uint64 { return (*uint64)(unsafe.Pointer(&r.mem[off])) }

// Drain hands every complete record written since the last call to fn, in order, and returns
// how many it read. A record not yet wholly visible stops the drain, and the next call reads it.
// A record that never verifies, from a writer that is gone, is the caller's to abandon (Skip).
func (r *Ring) Drain(fn func(payload []byte)) (n int) {
	head := atomic.LoadUint64(r.u64(oHead))
	tail := atomic.LoadUint64(r.u64(oTail))
	if head <= tail {
		return 0
	}
	// What is unread is counted from the tail, never compared by position, so a header whose
	// counters are near their limit cannot wrap a reader round forever; and a writer never has
	// more than a ring's worth unread, so a drain reads at most that. Positions wrap harmlessly:
	// the data size is a power of two.
	avail := min(head-tail, r.dataSize)
	data := r.mem[Header : Header+r.dataSize]
	le := binary.LittleEndian
	for done := uint64(0); done < avail; {
		pos := (tail + done) % r.dataSize
		if r.dataSize-pos < 8 {
			break // the writer keeps records 8-aligned; this cannot happen in a well-formed ring
		}
		length := le.Uint32(data[pos:])
		if length == skip {
			done += r.dataSize - pos
			continue
		}
		need := align(8 + uint64(length))
		if uint64(length) > maxRecord || pos+need > r.dataSize || need > avail-done {
			break // not yet visible, or not a record
		}
		payload := data[pos+8 : pos+8+uint64(length)]
		if crc32.ChecksumIEEE(payload) != le.Uint32(data[pos+4:]) {
			break
		}
		fn(append([]byte(nil), payload...))
		n++
		done += need
		atomic.StoreUint64(r.u64(oTail), tail+done)
	}
	return n
}

// Pending is how many bytes are written but not yet consumed.
func (r *Ring) Pending() uint64 {
	return atomic.LoadUint64(r.u64(oHead)) - atomic.LoadUint64(r.u64(oTail))
}

// Skip abandons whatever is left unread: for a ring whose writer is gone and whose next record
// will never verify. It returns the bytes abandoned.
func (r *Ring) Skip() uint64 {
	head := atomic.LoadUint64(r.u64(oHead))
	tail := atomic.LoadUint64(r.u64(oTail))
	atomic.StoreUint64(r.u64(oTail), head)
	return head - tail
}

func align(n uint64) uint64 { return (n + 7) &^ 7 }

// Kind of a slot.
const (
	Progress = 1
	Metric   = 2
)

// Slot flags.
const (
	fB, fC, fD, fE, fANull, fAInt = 1, 2, 4, 8, 16, 32
)

// Slot is one latest value, read consistently.
type Slot struct {
	Index       int
	Seq         uint64
	Kind        int
	Key         string
	A           float64 // done, or a metric's value
	AInt, ANull bool
	B           float64 // total
	C           int64   // sim, or a metric's at
	D, E        float64 // rate, eta
	HasB, HasC  bool
	HasD, HasE  bool
	T           float64
	// After is the sequence number of the last record written before this value: show it only
	// once the ring has been read that far.
	After uint64
}

// Slots returns every slot whose sequence number moved past last[i], read under its lock. A slot
// mid-write is left for the next call.
func (r *Ring) Slots(last []uint64) []Slot {
	used := atomic.LoadUint64(r.u64(oSlotsUsed))
	if used > r.slotCount {
		used = r.slotCount
	}
	var out []Slot
	le := binary.LittleEndian
	for i := uint64(0); i < used; i++ {
		off := int(r.slotsOff + i*slotSize)
		seq := atomic.LoadUint64(r.u64(off))
		if seq == 0 || seq&1 == 1 || (int(i) < len(last) && seq <= last[i]) {
			continue
		}
		var body [slotSize]byte
		copy(body[:], r.mem[off:off+slotSize])
		if atomic.LoadUint64(r.u64(off)) != seq {
			continue // written meanwhile: next time
		}
		keylen := int(le.Uint16(body[14:]))
		if keylen > slotSize-slotKeyOff {
			continue
		}
		if crc32.ChecksumIEEE(body[slotBodyOff:slotKeyOff+keylen]) != le.Uint32(body[8:]) {
			continue
		}
		flags := body[13]
		s := Slot{
			Index: int(i), Seq: seq, Kind: int(body[12]), Key: string(body[slotKeyOff : slotKeyOff+keylen]),
			A: math.Float64frombits(le.Uint64(body[16:])), B: math.Float64frombits(le.Uint64(body[24:])), C: int64(le.Uint64(body[32:])),
			D: math.Float64frombits(le.Uint64(body[40:])), E: math.Float64frombits(le.Uint64(body[48:])), T: math.Float64frombits(le.Uint64(body[56:])),
			After: le.Uint64(body[64:]),
			HasB:  flags&fB != 0, HasC: flags&fC != 0, HasD: flags&fD != 0, HasE: flags&fE != 0, ANull: flags&fANull != 0, AInt: flags&fAInt != 0,
		}
		out = append(out, s)
	}
	return out
}

// Counters are what the writer counted.
type Counters struct {
	Dropped, Truncated, Limited, SlotsFull uint64
	Closed                                 bool
}

// Counters reads the writer's counters.
func (r *Ring) Counters() Counters {
	return Counters{
		Dropped: atomic.LoadUint64(r.u64(oDropped)), Truncated: atomic.LoadUint64(r.u64(oTruncated)), Limited: atomic.LoadUint64(r.u64(oLimited)),
		SlotsFull: atomic.LoadUint64(r.u64(oSlotsFull)), Closed: atomic.LoadUint64(r.u64(oClosed)) == 1,
	}
}

// Event turns a slot into a progress or metric event's fields (without the envelope).
func (s Slot) Event() map[string]any {
	a := any(s.A)
	switch {
	case s.ANull:
		a = nil
	case s.AInt:
		a = int64(s.A)
	}
	if s.Kind == Metric {
		e := map[string]any{"kind": "metric", "name": s.Key, "value": a, "t": s.T, "slot": s.Index, "sseq": s.Seq}
		if s.HasC {
			e["at"] = s.C
		}
		return e
	}
	e := map[string]any{"kind": "progress", "key": s.Key, "done": a, "t": s.T, "slot": s.Index, "sseq": s.Seq}
	if s.HasB {
		e["total"] = s.B
	}
	if s.HasC {
		e["sim"] = s.C
	}
	if s.HasD {
		e["rate"] = s.D
	}
	if s.HasE {
		e["eta"] = s.E
	}
	return e
}
