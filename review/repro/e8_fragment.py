import math, pathlib, statistics, sys, time
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[2] / "python"))
from tests.test_nautilus import engine, quotes, outcome, BASE, SEC   # the tests' own engine and data
import gongoozler as gz
from gongoozler import nautilus as gzn

N = 1_000_000
aud = quotes("AUD/USD", [BASE + i * SEC for i in range(N)])
eur = quotes("EUR/USD", [BASE + i * 60 * SEC for i in range(N // 60)], 1.3)   # one quote a minute

def once():
    e, r = engine(["AUD/USD", "EUR/USD"])
    t = time.perf_counter(); e.add_data(aud); e.add_data(eur); e.run(); dt = time.perf_counter() - t  # loading counted, as when streaming
    out = outcome(e, r)[:3]; e.dispose(); return dt, out, 1

def streamed(sparse_batch):
    e, r = engine(["AUD/USD", "EUR/USD"])
    feeds = [gz.from_items("aud", aud, batch_size=100_000), gz.from_items("eur", eur, batch_size=sparse_batch)]
    t = time.perf_counter(); s = gzn.run(e, feeds, snapshot=False); dt = time.perf_counter() - t
    out = outcome(e, r)[:3]; e.dispose(); return dt, out, s.windows

def streamed_ts(sparse_batch):
    e, r = engine(["AUD/USD", "EUR/USD"])
    feeds = [gz.from_items("aud", aud, batch_size=100_000, ts=[q.ts_init for q in aud]), gz.from_items("eur", eur, batch_size=sparse_batch, ts=[q.ts_init for q in eur])]
    t = time.perf_counter(); s = gzn.run(e, feeds, snapshot=False); dt = time.perf_counter() - t
    out = outcome(e, r)[:3]; e.dispose(); return dt, out, s.windows

base = None
for name, fn in [("one run, both feeds added whole", once), ("streamed, sparse feed 1,000 per batch", lambda: streamed(1_000)),
                 ("streamed, sparse feed 10 per batch", lambda: streamed(10)), ("streamed, sparse feed 1 per batch", lambda: streamed(1)), ("streamed, timestamps handed over, 1,000", lambda: streamed_ts(1_000))]:
    runs = [fn() for _ in range(3)]
    dt = statistics.median(r[0] for r in runs); out, windows = runs[0][1], runs[0][2]
    base = base or (dt, out)
    print(f"{name:40} {dt:6.2f}s  {windows:6} windows  {dt / base[0]:4.1f}x  same result: {out == base[1]}")
