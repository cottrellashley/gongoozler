"""A second, independent reader of spec/ring.md, for the tests: it checks the Python writer
against the spec as the Go reader does."""

from __future__ import annotations

import glob
import json
import mmap
import os
import struct
import zlib
from dataclasses import dataclass

from gongoozler import _ring as R


@dataclass
class Slot:
    index: int
    seq: int
    kind: int
    key: str
    a: float
    b: float
    c: int
    d: float
    e: float
    t: float
    after: int
    flags: int


class Reader:
    def __init__(self, path: str) -> None:
        self.path = path
        f = open(path, "r+b")  # noqa: SIM115
        self.mm = mmap.mmap(f.fileno(), 0)
        f.close()
        assert self.mm[0:8] == R.MAGIC
        self.data_size, self.slots_off, self.slot_count, self.slot_size = struct.unpack_from("<QQII", self.mm, R.O_DATA_SIZE)
        self.pid = struct.unpack_from("<I", self.mm, R.O_PID)[0]

    def u64(self, off: int) -> int:
        return struct.unpack_from("<Q", self.mm, off)[0]

    def drain(self) -> list[dict]:
        head, tail = self.u64(R.O_HEAD), self.u64(R.O_TAIL)
        out = []
        while tail < head:
            pos = tail % self.data_size
            length = struct.unpack_from("<I", self.mm, R.HEADER + pos)[0]
            if length == R.SKIP:
                tail += self.data_size - pos
                continue
            crc = struct.unpack_from("<I", self.mm, R.HEADER + pos + 4)[0]
            payload = bytes(self.mm[R.HEADER + pos + 8 : R.HEADER + pos + 8 + length])
            assert zlib.crc32(payload) == crc, "a record failed its check"
            out.append(json.loads(payload))
            tail += (8 + length + 7) & ~7
        struct.pack_into("<Q", self.mm, R.O_TAIL, tail)
        return out

    def slots(self) -> list[Slot]:
        out = []
        for i in range(self.u64(R.O_SLOTS_USED)):
            off = self.slots_off + i * self.slot_size
            seq = self.u64(off)
            body = bytes(self.mm[off : off + self.slot_size])
            assert seq % 2 == 0 and self.u64(off) == seq
            kind, flags, keylen, a, b, c, d, e, t, after = R._SLOT.unpack_from(body, 12)
            assert zlib.crc32(body[12 : 80 + keylen]) == struct.unpack_from("<I", body, 8)[0], "a slot failed its check"
            out.append(Slot(i, seq, kind, body[80 : 80 + keylen].decode(), a, b, c, d, e, t, after, flags))
        return out

    def counters(self) -> dict[str, int]:
        names = {"dropped": R.O_DROPPED, "truncated": R.O_TRUNCATED, "limited": R.O_LIMITED, "closed": R.O_CLOSED, "slots_full": R.O_SLOTS_FULL}
        return {k: self.u64(v) for k, v in names.items()}

    def close(self) -> None:
        self.mm.close()


def rings(directory: str) -> list[Reader]:
    files = sorted(glob.glob(os.path.join(directory, "*.ring")), key=os.path.getmtime)
    return [Reader(f) for f in files]
