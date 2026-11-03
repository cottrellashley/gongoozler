# /// script
# requires-python = ">=3.12"
# dependencies = ["gongoozler"]
# [tool.uv.sources]
# gongoozler = { path = "../../python", editable = true }
# ///
import signal, time, gongoozler as gz
got = []
for s in (signal.SIGINT, signal.SIGTERM):
    signal.signal(s, lambda n, f: got.append(signal.Signals(n).name))
open("/tmp/e5.ready", "w").write("1")
t = time.time()
while time.time() - t < 2.5:
    time.sleep(0.02)
print("received:", got)
