"""P1. A coupon is counted twice, silently, when the supplied accrued interest does not reset.

Contracts have no terms, so accrued interest and coupons are both supplied data, and nothing
checks one against the other. Here a bond's accrued interest climbs by 0.01 a day for 60 days;
a coupon of 0.30 per unit is paid on day 30, but the data's accrued interest carries on climbing
instead of falling back to zero. Income is then ΔA + coupon: the 30 days of accrual are counted
once as accrued interest and again as the coupon. The run reports no error and an identity error
of zero, because the identity holds for whatever data it is given.

Run: engine/.venv/bin/python review/engine-repro/p1_coupon_counted_twice.py

Fixed: a supplied bond now checks that its accrued interest falls back when a coupon pays, and
refuses the second series; a bond defined by its terms has one schedule for both.
"""

import datetime as dt

import polars as pl

import fi_engine as fi

days = 60
dates = []
d = dt.date(2025, 1, 6)
while len(dates) < days:
    if d.weekday() < 5:
        dates.append(d)
    d += dt.timedelta(days=1)

for name, accrued in [("resets on the coupon", [0.01 * (k % 30) for k in range(days)]), ("never resets", [0.01 * k for k in range(days)])]:
    ws = fi.Workspace()
    ws.series("marks", pl.DataFrame({"isin": ["B1"] * days, "date": dates, "clean": [100.0] * days, "accrued": accrued}),
              entity="isin", kind="bond", time="date", values={"clean": "price", "accrued": "price"}, grid=True)
    ws.series("flows", pl.DataFrame({"isin": ["B1"], "date": [dates[30]], "interest": [0.30 / 100]}),  # 0.30 per 100 of face, per unit of face
              entity="isin", kind="bond", time="date", values={"interest": "number"})
    try:
        rec = ws.records(ws.supplied_bonds("marks", flows="flows"), ws.calendar("marks"))
    except fi.InvalidError as e:
        print(f"accrued {name:>21}: refused: {e}")
        continue
    once = fi.Decision.python(lambda ctx: {"B1": 1.0} if ctx.k == 0 else None, name="once")
    run = ws.run(fi.Strategy(once, schedule="all", execution_lag=0), rec)
    led = run.ledger()
    print(f"accrued {name:>21}: income {led['income'].sum():14,.2f}   identity error {run.max_identity_error:.1e}")
print("The bond accrued 0.59 per 100 over the period either way; before the fix the second run booked 0.89.")
