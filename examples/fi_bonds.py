# /// script
# requires-python = ">=3.13"
# dependencies = ["gongoozler[fi]", "numpy", "polars"]
# [tool.uv.sources]
# gongoozler = { path = "../python", editable = true }
# fi-engine = { path = "../engine", editable = true }
# [tool.uv]
# exclude-newer = "2026-11-01T00:00:00Z"
# [tool.gongoozler]
# memory = "4GiB"                  # the run needs well under 1 GiB; the first after a change to the engine builds it
# sweep = { top = [10, 20, 40] }
# ///
"""A fixed income backtest in the engine built into gongoozler: bonds defined by their terms,
priced off a government curve and their own spreads, held by a carry strategy, watched as it runs.

The data here is a stand-in, simulated: a terms table (fixed coupons, floaters, callables), a curve
that moves in level and slope, a spread per bond, the floaters' fixings. Any data library hands
the engine frames like these; the engine computes every coupon, accrual, call and redemption from
the terms.

    uv run examples/fi_bonds.py                          # progress printed in the terminal
    gongoozler run examples/fi_bonds.py                  # watched by the CLI; its results kept
    gongoozler run --param top=40 examples/fi_bonds.py   # a parameter set from outside
    gongoozler sweep examples/fi_bonds.py --rank run.total_return

The engine is a path source here (`../engine`): the first run after a change to its Rust builds it,
which takes minutes; every later run starts at once.
"""

import numpy as np
import polars as pl

import fi_engine as fi
import gongoozler as gz
import gongoozler.fi as gzfi

p = gz.params(bonds=200, top=20, seed=7, cost_bp=3.0)
rng = np.random.default_rng(p.seed)
cal = fi.Calendar.weekdays("2021-01-04", "2025-12-31")
dates = cal.dates()
days = len(dates)

# 1. The universe, from a terms table, as a desk keeps one.
names = [f"B{i:04d}" for i in range(p.bonds)]
issue = [dates[0].replace(year=dates[0].year - int(rng.integers(0, 3)), month=int(rng.integers(1, 13)), day=15) for _ in names]
life = rng.integers(3, 25, p.bonds)
kind = rng.choice(["fixed", "floating", "callable"], p.bonds, p=[0.7, 0.15, 0.15])
terms = pl.DataFrame(
    {
        "name": names,
        "issue": issue,
        "maturity": [d.replace(year=d.year + int(n)) for d, n in zip(issue, life, strict=True)],
        "day_count": ["30/360" if k != "floating" else "ACT/360" for k in kind],
        "frequency": [2 if k != "floating" else 4 for k in kind],
        "coupon": [None if k == "floating" else float(np.round(rng.uniform(0.01, 0.07), 4)) for k in kind],
        "index": ["TERM3M" if k == "floating" else None for k in kind],
        "spread": [float(np.round(rng.uniform(0.005, 0.02), 4)) if k == "floating" else None for k in kind],
        "calendar": ["NY"] * p.bonds,
        "rounding": [7] * p.bonds,
    }
)
callable_ = terms.filter(pl.Series(kind == "callable"))
rights = pl.DataFrame(
    {
        "name": callable_["name"],
        "kind": ["call"] * callable_.height,
        "from": [m.replace(year=m.year - 2) for m in callable_["maturity"]],
        "to": callable_["maturity"],
        "price": [100.0] * callable_.height,
    }
)
holidays = fi.BusinessCalendar.from_rules("NY", [("fixed", 1, 1, "nearest"), ("fixed", 7, 4, "nearest"), ("fixed", 12, 25, "nearest")], (2010, 2050))
bonds = fi.bonds(terms, calendars={"NY": holidays}, rights=rights)

# 2. The market: a government curve that moves in level and slope, a spread for each bond, and the
#    term rate the floaters fix on.
tenors = {"z0_25": 0.25, "z1": 1.0, "z2": 2.0, "z5": 5.0, "z10": 10.0, "z30": 30.0}
level = 0.02 + np.cumsum(rng.normal(0, 0.0004, days))
slope = 0.01 + np.cumsum(rng.normal(0, 0.0002, days))
curve = pl.DataFrame({"curve": ["GOV"] * days, "date": dates, **{c: level + slope * (1 - np.exp(-t / 3.0)) for c, t in tenors.items()}})
spread = np.maximum(0.001, rng.uniform(0.005, 0.03, p.bonds) + np.cumsum(rng.normal(0, 0.0002, (days, p.bonds)), axis=0))
alive = np.array([[d < m for m in terms["maturity"]] for d in dates])
spreads = pl.DataFrame(
    {
        "isin": names * days,
        "date": [d for d in dates for _ in names],
        "spread": spread.ravel(),
        # What the strategy ranks on: the spread of a bond still outstanding.
        "carry": np.where(alive, spread, np.nan).ravel(),
    }
)
# The term rate's history goes back before the run: a floater mid-period at the start needs the
# fixing its period was set on.
history = fi.Calendar.weekdays("2017-01-02", "2020-12-31").dates()
fixings = pl.DataFrame({"index": "TERM3M", "date": history + dates, "rate": np.r_[np.full(len(history), level[0]), level]})

ws = fi.Workspace()
ws.series("curve", curve, entity="curve", kind="curve", time="date", values=dict.fromkeys(tenors, "number"), grid=True)
# Spreads are known the evening they are marked; the strategy ranks on them the next day.
ws.series("spreads", spreads, entity="isin", kind="bond", time="date", values={"spread": "number", "carry": "number"}, grid=True, known=fi.lag(hours=18), nan_is_missing=True)
ws.series("fixings", fixings, entity="index", kind="index", time="date", values={"rate": "number"}, grid=True)

# 3. The contracts, valued off the curve plus their own spread; floaters fix on the term rate.
contracts = ws.debt(bonds, fi.valuation.curve("curve", "GOV", tenors, spread="spreads.spread"), fixings="fixings.rate")
records = ws.records(contracts, cal)

# 4. The strategy: each month end, the widest spreads, equal weights (carry); fills cost cost_bp.
book = fi.Strategy(fi.Decision.top_n("spreads.carry", p.top), fill=fi.Fill.research(p.cost_bp), journal=True, name="carry")
run = gzfi.run(ws, book, records)
report = run.report()
gz.log(f"carry top {p.top}: total return {run.total_return:+.2%}, Sharpe {report['sharpe']:.2f}, {run.trades} trades")
print(f"total return {run.total_return:+.2%}, income {report['income']:,.0f}, price {report['price']:,.0f}, identity error {run.max_identity_error:.1e}")
