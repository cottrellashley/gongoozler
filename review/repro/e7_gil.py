import time, gongoozler as gz
def work(n):
    x = 0
    for i in range(n):
        x += i * i % 7
    return x
work(10_000)
t = time.perf_counter(); [work(100) for _ in range(50_000)]; base = time.perf_counter() - t
t = time.perf_counter()
for c in range(50_000):
    work(100)
    gz.emit("tick", i=c)
both = time.perf_counter() - t
t = time.perf_counter()
for c in range(50_000):
    gz.emit("tick2", i=c)
alone = time.perf_counter() - t
print(f"strategy alone {base:.3f}s; with an event every 100 iterations {both:.3f}s; the emit calls alone {alone:.3f}s; "
      f"unexplained slowdown {100 * (both - base - alone) / base:+.1f}%")
