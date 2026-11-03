"""The shared-memory ring a run writes its events into: the writer's side of spec/ring.md.

One ring per process, in a file the supervisor reads from outside the process:
- **ordered events** are records in a ring: `[u32 length][u32 crc32][payload]`, padded to 8 bytes;
- **progress and metrics** are latest-value slots, each guarded by a sequence lock and a crc32.

The writer never waits. When the ring is full, the record is dropped and counted in the header.
Nothing here allocates a thread, and what was written survives the process: the supervisor drains
the file after a crash. All integers are little-endian.
"""

from __future__ import annotations

import mmap
import os
import secrets
import struct
import time
import zlib
from pathlib import Path

MAGIC = b"GZRING01"
VERSION = 1
HEADER = 4096
SKIP = 0xFFFFFFFF
SLOT_SIZE = 256
SLOT_KEY_MAX = SLOT_SIZE - 80

# Header fields (byte offsets).
O_MAGIC = 0
O_VERSION = 8  # u32
O_HEADER = 12  # u32
O_DATA_SIZE = 16  # u64
O_SLOTS_OFF = 24  # u64
O_SLOT_COUNT = 32  # u32
O_SLOT_SIZE = 36  # u32
O_PID = 40  # u32
O_CREATED = 48  # u64, nanoseconds since the epoch
O_HEAD = 64  # u64: bytes written, ever (the writer's)
O_TAIL = 128  # u64: bytes consumed, ever (the reader's)
O_DROPPED = 192  # u64: records dropped because the ring was full
O_TRUNCATED = 200  # u64: events cut down to fit
O_LIMITED = 208  # u64: events refused by a rate limit
O_CLOSED = 216  # u64: 1 once the writer closed cleanly
O_SLOTS_USED = 224  # u64
O_SLOTS_FULL = 232  # u64: updates refused because every slot was taken

KIND_PROGRESS = 1
KIND_METRIC = 2
# Slot flags: which optional fields are present, and whether `a` was an integer.
F_B, F_C, F_D, F_E, F_A_NULL, F_A_INT = 1, 2, 4, 8, 16, 32

_U64 = struct.Struct("<Q")
_REC = struct.Struct("<II")
# A slot: seq u64 at 0, crc32 u32 at 8, then the body the crc covers: kind u8, flags u8, keylen u16,
# a f64, b f64, c i64, d f64, e f64, t f64, after u64 (offsets 12 to 72), 8 reserved bytes, the key
# at 80. `after` is the sequence number of the last record written before this update: a reader
# shows the update only once it has read that far, so an update never appears before an event
# that preceded it.
_SLOT = struct.Struct("<BBHddqdddQ")
_RESERVED = bytes(8)


def _align(n: int) -> int:
    return (n + 7) & ~7


class Ring:
    """A writer. Not thread-safe by itself: the emitter calls it under its own lock."""

    def __init__(self, directory: str | os.PathLike[str], *, data_size: int = 8 << 20, slot_count: int = 1024) -> None:
        if data_size & (data_size - 1) or data_size < 1 << 16:
            raise ValueError("data_size must be a power of two of at least 64 KiB")
        self.data_size = data_size
        self.slot_count = slot_count
        self.slots_off = HEADER + data_size
        size = self.slots_off + slot_count * SLOT_SIZE
        self.path = Path(directory) / f"{os.getpid()}-{secrets.token_hex(4)}.ring"
        fd = os.open(self.path, os.O_CREAT | os.O_EXCL | os.O_RDWR, 0o600)
        try:
            os.ftruncate(fd, size)
            self.mm = mmap.mmap(fd, size, mmap.MAP_SHARED, mmap.PROT_READ | mmap.PROT_WRITE)
        finally:
            os.close(fd)
        mm = self.mm
        struct.pack_into("<II", mm, O_VERSION, VERSION, HEADER)
        struct.pack_into("<QQII", mm, O_DATA_SIZE, data_size, self.slots_off, slot_count, SLOT_SIZE)
        struct.pack_into("<I", mm, O_PID, os.getpid())
        _U64.pack_into(mm, O_CREATED, time.time_ns())
        mm[O_MAGIC : O_MAGIC + 8] = MAGIC  # last: a reader seeing the magic sees a whole header
        self.head = 0
        self.slots: dict[tuple[int, str], int] = {}
        self.slot_seq = [0] * slot_count

    # ----------------------------------------------------------------------------- records

    def write(self, payload: bytes) -> bool:
        """Appends one record. False (and counted) when it does not fit."""
        need = _align(8 + len(payload))
        if need > self.data_size // 4:
            self.count(O_TRUNCATED)
            return False
        mm = self.mm
        tail = _U64.unpack_from(mm, O_TAIL)[0]
        pos = self.head % self.data_size
        wrap = self.data_size - pos if pos + need > self.data_size else 0
        if self.data_size - (self.head - tail) < wrap + need:
            self.count(O_DROPPED)
            return False
        if wrap:
            struct.pack_into("<I", mm, HEADER + pos, SKIP)
            self.head += wrap
            pos = 0
        base = HEADER + pos
        mm[base + 8 : base + 8 + len(payload)] = payload
        _REC.pack_into(mm, base, len(payload), zlib.crc32(payload))
        self.head += need
        _U64.pack_into(mm, O_HEAD, self.head)
        return True

    # ------------------------------------------------------------------------------- slots

    def slot(self, kind: int, key: str) -> int | None:
        """The slot holding `key`'s latest value, taking a free one the first time."""
        k = (kind, key)
        i = self.slots.get(k)
        if i is None:
            if len(self.slots) >= self.slot_count:
                self.count(O_SLOTS_FULL)
                return None
            i = len(self.slots)
            self.slots[k] = i
            _U64.pack_into(self.mm, O_SLOTS_USED, len(self.slots))
        return i

    def set(
        self, i: int, kind: int, key: bytes, a: float | int | None, b: float | None, c: int | None, d: float | None, e: float | None, t: float, after: int
    ) -> int:
        """Writes slot i, and returns its new sequence number (always even)."""
        flags = 0
        if a is None:
            flags |= F_A_NULL
            a = 0.0
        elif isinstance(a, int) and not isinstance(a, bool):
            flags |= F_A_INT
        if b is not None:
            flags |= F_B
        if c is not None:
            flags |= F_C
        if d is not None:
            flags |= F_D
        if e is not None:
            flags |= F_E
        key = key[:SLOT_KEY_MAX]
        body = _SLOT.pack(kind, flags, len(key), float(a), b or 0.0, c or 0, d or 0.0, e or 0.0, t, after) + _RESERVED + key
        crc = zlib.crc32(body)
        off = self.slots_off + i * SLOT_SIZE
        seq = self.slot_seq[i] + 1
        mm = self.mm
        _U64.pack_into(mm, off, seq)  # odd: being written
        mm[off + 12 : off + 12 + len(body)] = body
        struct.pack_into("<I", mm, off + 8, crc)
        seq += 1
        _U64.pack_into(mm, off, seq)  # even: stable
        self.slot_seq[i] = seq
        return seq

    # ----------------------------------------------------------------------------- counters

    def count(self, offset: int, n: int = 1) -> None:
        _U64.pack_into(self.mm, offset, _U64.unpack_from(self.mm, offset)[0] + n)

    def close(self) -> None:
        try:
            _U64.pack_into(self.mm, O_CLOSED, 1)
            self.mm.flush()
            self.mm.close()
        except (ValueError, OSError):
            pass
