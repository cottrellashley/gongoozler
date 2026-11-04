# /// script
# requires-python = ">=3.13"
# dependencies = ["gongoozler[fi]", "numpy", "polars"]
# [tool.uv]
# exclude-newer = "2026-11-01T00:00:00Z"
# [tool.gongoozler]
# memory = "2GiB"
# cpus = 2
# timeout = "10m"
# ///
"""A file for a box: it names gongoozler (with its engine) and no path, as a box resolves both from
the wheels in its image (Dockerfile). A smaller universe than examples/fi_bonds.py: bonds from their
terms, priced off a curve and a spread each, held for carry, results kept.

    gongoozler run --remote examples/fi_in_a_box.py     # on the server, in a container of its own
"""

import numpy as np
import polars as pl

import fi_engine as fi
import gongoozler as gz
import gongoozler.fi as gzfi

p = gz.params(bonds=60, top=10, seed=11)
rng = np.random.default_rng(p.seed)
cal = fi.Calendar.weekdays("2023-01-02", "2024-12-31")
dates = cal.dates()
names = [f"B{i:03d}" for i in range(p.bonds)]
terms = pl.DataFrame(
    {
        "name": names,
        "issue": [dates[0].replace(month=int(rng.integers(1, 13)), day=1, year=2022) for _ in names],
        "maturity": [dates[0].replace(year=2024 + int(rng.integers(0, 15)), day=1) for _ in names],
        "day_count": "30/360",
        "frequency": 2,
        "coupon": np.round(rng.uniform(0.01, 0.06, p.bonds), 4),
    }
)
bonds = fi.bonds(terms)
tenors = {"z1": 1.0, "z5": 5.0, "z10": 10.0, "z30": 30.0}
level = 0.03 + np.cumsum(rng.normal(0, 0.0004, len(dates)))
ws = fi.Workspace()
ws.series("curve", pl.DataFrame({"curve": "GOV", "date": dates, **{c: level + 0.001 * t**0.5 for c, t in tenors.items()}}), entity="curve", kind="curve",
          time="date", values=dict.fromkeys(tenors, "number"), grid=True)
spread = np.maximum(0.001, rng.uniform(0.005, 0.03, p.bonds) + np.cumsum(rng.normal(0, 0.0002, (len(dates), p.bonds)), axis=0))
ws.series("spreads", pl.DataFrame({"isin": names * len(dates), "date": [d for d in dates for _ in names], "spread": spread.ravel()}), entity="isin",
          kind="bond", time="date", values={"spread": "number"}, grid=True, known=fi.lag(hours=18))
records = ws.records(ws.debt(bonds, fi.valuation.curve("curve", "GOV", tenors, spread="spreads.spread")), cal)
run = gzfi.run(ws, fi.Strategy(fi.Decision.top_n("spreads.spread", p.top), fill=fi.Fill.research(3.0), name="carry"), records)
print(f"total return {run.total_return:+.2%} in a box; identity error {run.max_identity_error:.1e}")
