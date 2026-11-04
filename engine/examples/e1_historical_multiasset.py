# /// script
# requires-python = ">=3.13"
# dependencies = ["fi-engine", "numpy", "polars"]
# [tool.uv.sources]
# fi-engine = { path = "..", editable = true }
# [tool.uv]
# exclude-newer = "2026-11-01T00:00:00Z"
# ///
"""E1: a historical multi-asset policy replay, point in time (the brief's example 1, in Python).

Two policies share one USD book:
- an allocation into the eight Treasuries with the best carry, and EUR corporates;
- a Treasury futures hedge, on its own clock.

The book has these features:
- EUR bonds settle in two days, everything else in one;
- foreign cash converts automatically, and EUR exposure is a soft limit;
- the carry signal is published the morning after.

The data is synthetic; a desk brings its own frames.

Run it with: .venv/bin/python examples/e1_historical_multiasset.py
"""

import numpy as np
import polars as pl

import fi_engine as fi

rng = np.random.default_rng(11)
cal = fi.Calendar.weekdays("2021-01-04", "2022-12-30")
dates = cal.dates()
n = len(dates)


def each(days: list, count: int) -> list:
    """Every day `count` times: the time column of a long frame."""
    return [d for d in days for _ in range(count)]


def bonds(prefix: str, count: int, coupon: float) -> tuple[list[str], pl.DataFrame, pl.DataFrame]:
    names = [f"{prefix}{j:02d}" for j in range(count)]
    clean = 100 + np.cumsum(rng.normal(0, 0.12, (n, count)), axis=0)
    accrued = np.tile((np.arange(n) % 126)[:, None] * coupon / 2 * 100 / 126, (1, count))
    marks = pl.DataFrame({"bond": np.tile(names, n), "date": each(dates, count), "clean": clean.ravel(), "accrued": accrued.ravel()})
    paid = [d for k, d in enumerate(dates) if k and k % 126 == 0]
    flows = pl.DataFrame({"bond": np.tile(names, len(paid)), "date": each(paid, count), "interest": coupon / 2})
    return names, marks, flows


ws = fi.Workspace()  # decisions see each value as it was known
usts, ust_marks, ust_flows = bonds("UST", 20, 0.02)
eurs, eur_marks, eur_flows = bonds("EC", 10, 0.035)
for name, frame in [("ust", ust_marks), ("eur", eur_marks)]:
    ws.series(f"{name}_marks", frame, entity="bond", kind="bond", time="date", values={"clean": "price", "accrued": "price"}, grid=True)
for name, frame in [("ust", ust_flows), ("eur", eur_flows)]:
    ws.series(f"{name}_flows", frame, entity="bond", kind="bond", time="date", values={"interest": "number"})
ws.series("fx", pl.DataFrame({"ccy": "EUR", "date": dates, "rate": 1.15 * np.exp(np.cumsum(rng.normal(0, 0.004, n)))}),
          entity="ccy", kind="currency", time="date", values={"rate": "number"}, grid=True)
ws.series("fut", pl.DataFrame({"contract": "TY", "date": dates, "price": 130 + np.cumsum(rng.normal(0, 0.3, n))}),
          entity="contract", kind="future", time="date", values={"price": "number"}, grid=True)
ws.series("sofr", pl.DataFrame({"date": dates, "rate": np.full(n, 0.25)}), time="date", values={"rate": "%"})
# The carry signal for each Treasury, published the next morning (an assumed lag, reported as one).
carry = pl.DataFrame({"bond": np.tile(usts, n), "date": each(dates, len(usts)), "carry": rng.normal(0, 1, n * len(usts))})
ws.series("carry", carry, entity="bond", kind="bond", time="date", known=fi.lag(hours=18), values={"carry": "number"}, grid=True)

contracts = (ws.supplied_bonds("ust_marks", flows="ust_flows") + ws.supplied_bonds("eur_marks", flows="eur_flows", currency="EUR")
             + ws.daily_future("TY", "fut.price", 1000.0))
# The run requires coupons from its bond definitions: preparing it refuses one that cannot.
records = ws.records(contracts, cal, requires={"supplied_bond": ["coupon", "supplied marks"]})


@fi.decision
def allocate(ctx):
    score = np.nan_to_num(ctx.cross_section("carry", "carry", entities=usts), nan=-np.inf)
    best = [usts[i] for i in np.argsort(-score, kind="stable")[:8]]
    return {b: 0.6 / 8 for b in best} | {e: 0.3 / len(eurs) for e in eurs}


@fi.decision
def hedge(ctx):
    return fi.quantities({"TY": -40.0})


book = fi.Book(
    [fi.Policy(allocate, schedule="month_ends", lag=1, scope=usts + eurs, name="allocate"),
     fi.Policy(hedge, schedule=cal.every(63), lag=1, scope=["TY"], priority=1, name="hedge")],
    execution=fi.Fill.research(half_spread_bp=3.0),
    currency="USD", fx="fx.rate", settlement={"default": 1, **dict.fromkeys(eurs, 2)}, deposit_rate="sofr.rate",
    constraints=[fi.Constraint.soft("eur_cap", penalty=1.0, contracts=eurs, max=0.35, relative=True)],
    journal=True, name="multiasset",
)
run = ws.run(book, records)
led = run.ledger()
terms = {t: led[t].sum() for t in ["income", "price", "principal", "currency", "financing", "costs"]}
print(f"mode: {run.mode}; final NAV {run.final_nav:,.0f}; time-weighted return {run.total_return:+.2%}")
print("P&L by term: " + ", ".join(f"{k} {v:,.0f}" for k, v in terms.items()))
print(f"identity error {run.max_identity_error:.1e}; orders {run.order_stats['filled']} filled; EUR cap breaches {run.breaches[0]['count']}")
manifest = ws.manifest(run, book, records)
print(f"{manifest!r}")
