# /// script
# requires-python = ">=3.13"
# dependencies = ["fi-engine", "numpy", "polars"]
# [tool.uv.sources]
# fi-engine = { path = "..", editable = true }
# [tool.uv]
# exclude-newer = "2026-11-01T00:00:00Z"
# ///
"""Quickstart: frames in, a model, a strategy in Python and one built in, accounts out.

Run with the repo's environment (`make python && .venv/bin/python examples/quickstart.py`), or as a run
file, watched: `gongoozler run engine/examples/quickstart.py` (the engine reports its progress itself).
"""

import datetime as dt

import numpy as np
import polars as pl

import fi_engine as fi

# 1. Data, from anywhere: here, two years of daily marks for 200 bonds, built with NumPy.
rng = np.random.default_rng(7)
cal = fi.Calendar.weekdays("2024-01-01", "2025-12-31")
bonds, days = [f"US{j:04d}" for j in range(200)], len(cal)
marks = pl.DataFrame({
    "isin": np.tile(bonds, days),
    "date": np.repeat(np.array(cal.times(), dtype="datetime64[ns]"), len(bonds)),
    "clean": (100 + np.cumsum(rng.normal(0, 0.15, (days, len(bonds))), axis=0)).ravel(),
    "accrued": np.tile((np.arange(days) % 126)[:, None] * 0.02, (1, len(bonds))).ravel(),
    "oas": (150 + np.cumsum(rng.normal(0, 2, (days, len(bonds))), axis=0)).ravel(),
})
# The coupon (2.5 per 100) is paid as accrued interest resets, every 126 days.
paid = [t for k, t in enumerate(cal.times()) if k and k % 126 == 0]
flows = pl.DataFrame({"isin": np.tile(bonds, len(paid)), "date": np.repeat(np.array(paid, dtype="datetime64[ns]"), len(bonds)),
                      "interest": 0.025})  # cash per unit of face
sectors = pl.DataFrame({"isin": bonds, "sector": rng.choice(["fin", "ind", "utl"], len(bonds))})

# 2. Declare what each frame means. Marks are published the next morning.
ws = fi.Workspace()  # point in time: each value as first published
ws.series("marks", marks, entity="isin", kind="bond", time="date", known=fi.lag(hours=18),
          values={"clean": "price", "accrued": "price", "oas": "bp"}, grid=True)
ws.series("flows", flows, entity="isin", kind="bond", time="date", values={"interest": "number"})
ws.series("sector", sectors, entity="isin", kind="bond", values={"sector": "category"})

# 3. A model: OAS z-scored within each sector, every day. Its output is a series like any other.
ws.evaluate([fi.Model.zscore("cheap", "marks.oas", group="sector.sector")], ws.calendar("marks"))

# 4. Contracts step once into unit records; every strategy then shares them.
records = ws.records(ws.supplied_bonds("marks", flows="flows"), ws.calendar("marks"))


# 5. Two strategies: the cheapest 20 by z-score, built in and in Python, monthly, 5bp to trade.
@fi.decision
def cheapest(ctx):
    z = ctx.cross_section("cheap", "z")
    best = np.argsort(-np.nan_to_num(z, nan=-np.inf), kind="stable")[:20]
    return {ctx.contracts[i]: 1 / 20 for i in best}


fill = fi.Fill.research(half_spread_bp=5)
runs = ws.sweep([fi.Strategy(fi.Decision.top_n("cheap.z", 20), fill=fill), fi.Strategy(cheapest, fill=fill)], records)

# 6. Accounts: NAV, cash and P&L by term; statistics from Nautilus's analysis crate.
for run in runs:
    rep = run.report()
    print(f"{run.name:>14}: total return {run.total_return:+.2%}, Sharpe {rep['sharpe']:.2f}, "
          f"income {rep['income']:,.0f}, price {rep['price']:,.0f}, costs {rep['costs']:,.0f}, "
          f"identity error {run.max_identity_error:.0e}")
print(runs[0].ledger().select("time", "nav", "income", "price", "costs", "holdings").tail(3))
assert runs[0].version == runs[1].version, "the Python decision books the same accounts"
