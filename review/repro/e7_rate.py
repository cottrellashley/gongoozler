import sys, time, gongoozler as gz
N = int(sys.argv[1])
t = time.perf_counter()
for i in range(N):
    gz.emit("tick", i=i)              # ordered: never coalesced
print(f"emit loop: {N:,} events in {time.perf_counter() - t:.2f}s")
