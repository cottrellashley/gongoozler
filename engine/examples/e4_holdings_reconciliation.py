# /// script
# requires-python = ">=3.13"
# dependencies = ["fi-engine", "polars"]
# [tool.uv.sources]
# fi-engine = { path = "..", editable = true }
# [tool.uv]
# exclude-newer = "2026-11-01T00:00:00Z"
# ///
"""E4: holdings reconciliation (the brief's example 4, in Python).

Recorded trades replay through the book over January 2024 with these features:
- trades settle two days later;
- a coupon is paid while a purchase is unsettled, so the seller owes it to the book until
  settlement;
- a bond amortises.

The run's NAV is reconciled against an administrator's. Nothing is plugged: what the engine
does not explain stays visible. The run is a holdings replay, derived from its decisions rather
than declared.

Run it with: .venv/bin/python examples/e4_holdings_reconciliation.py
"""

import datetime as dt

import polars as pl

import fi_engine as fi

cal = fi.Calendar.weekdays("2024-01-02", "2024-01-31")
dates = cal.dates()
bonds = ["B1", "B2", "B3", "B4"]
d = {x.isoformat(): x for x in dates}

ws = fi.Workspace()
prices = {"B1": 99.5, "B2": 101.2, "B3": 100.0, "B4": 97.8}
ws.series("marks", pl.DataFrame({
    "bond": [b for _ in dates for b in bonds],
    "date": [x for x in dates for _ in bonds],
    "clean": [prices[b] + 0.02 * k for k, _ in enumerate(dates) for b in bonds],
    "accrued": [0.0] * (len(dates) * len(bonds)),
}), entity="bond", kind="bond", time="date", values={"clean": "price", "accrued": "price"}, grid=True)
# B1 pays a 2.0 coupon on the 15th; B3 repays 10% of its face at par on the 16th.
ws.series("flows", pl.DataFrame({
    "bond": ["B1", "B3"], "date": [d["2024-01-15"], d["2024-01-16"]], "interest": [0.02, 0.0], "principal_face": [0.0, 0.1],
}), entity="bond", kind="bond", time="date", values={"interest": "number", "principal_face": "number"})
# The trade blotter: units of face, positive buys. The B1 purchase on the 12th settles on the
# 16th, after the coupon.
ws.series("blotter", pl.DataFrame({
    "bond": ["B1", "B2", "B3", "B2", "B4", "B1"],
    "date": [d["2024-01-02"]] * 3 + [d["2024-01-10"]] * 2 + [d["2024-01-12"]],
    "qty": [100_000.0, 50_000.0, 80_000.0, -50_000.0, 60_000.0, 20_000.0],
}), entity="bond", kind="bond", time="date", values={"qty": "number"})

records = ws.records(ws.supplied_bonds("marks", flows="flows"), cal)
book = fi.Book([fi.Policy(fi.Decision.trades("blotter.qty"), schedule="all", lag=0, name="blotter")],
               initial_cash=300_000.0, settlement=2, journal=True, name="fund")
run = ws.run(book, records)
led = run.ledger()
print(f"mode: {run.mode}; NAV at month end {run.final_nav:,.2f}; identity error {run.max_identity_error:.1e}")
print("holdings:", {k: round(v) for k, v in run.final_holdings.items()})
owed = run.journal().filter(pl.col("note") == "owed by the seller until settlement")
print(f"coupon owed by B1's seller until settlement: {owed['amount'].sum():,.2f}")

# The administrator's NAV, as its files would give it. Here it is a stand-in that booked B3's
# paydown a day late, valuing the repaid face at the mark on the 16th.
admin = led.select(pl.col("time").dt.date().alias("date"), "nav").with_columns(
    pl.when(pl.col("date") == d["2024-01-16"]).then(pl.col("nav") + 8_000 * (100.30 - 100.0) / 100).otherwise(pl.col("nav")).alias("nav"))
ws.series("admin", admin, time="date", values={"nav": "number"})
rec = ws.reconcile(run, "admin.nav")
print("unexplained differences:")
print(rec.filter(pl.col("difference").abs() > 1e-6))
print(f"journal: {run.journal().group_by('kind').len().sort('kind').rows()}")
