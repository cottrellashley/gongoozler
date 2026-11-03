import time
from dataclasses import dataclass
import gongoozler as gz

@dataclass(frozen=True, slots=True)
class Item:
    ts_init: int

def burst(batches, size):
    """`batches` batches of `size` items, all at one timestamp, then one later item."""
    for _ in range(batches):
        yield [Item(0)] * size
    yield [Item(1)]

for batches in (50, 100, 200, 400):
    size = 2_500
    a = gz.feed("bars", lambda: burst(batches, size))
    b = gz.feed("other", [[Item(0), Item(5)]])
    t = time.perf_counter()
    n = sum(w.count for w in gz.windows([a, b], check="edges"))
    dt = time.perf_counter() - t
    print(f"{batches:4} same-timestamp batches ({batches * size:>9,} items): {dt * 1e3:8.1f} ms, {dt / (batches * size) * 1e9:6.0f} ns per item")
