import os, time
data = bytearray(200_000_000)            # 200 MB, written once so it is resident
for i in range(0, len(data), 4096):
    data[i] = 1
kids = []
for _ in range(3):                       # three workers share it copy-on-write, as multiprocessing's fork does
    pid = os.fork()
    if pid == 0:
        time.sleep(2.5)
        os._exit(0)
    kids.append(pid)
time.sleep(1.5)
pss = 0
for p in [os.getpid(), *kids]:           # the honest number: proportional set size
    for line in open(f"/proc/{p}/smaps_rollup"):
        if line.startswith("Pss:"):
            pss += int(line.split()[1]) * 1024
print(f"actual (summed PSS) {pss / 2**20:.0f} MiB")
for p in kids:
    os.waitpid(p, 0)
