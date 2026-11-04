# /// script
# requires-python = ">=3.13"
# dependencies = ["fi-engine", "polars"]
# [tool.uv.sources]
# fi-engine = { path = "..", editable = true }
# [tool.uv]
# exclude-newer = "2026-11-01T00:00:00Z"
# ///
"""E2: a CLO manager counterfactual (the brief's example 2, in Python).

A CLO's collateral book replays its manager's recorded trades. The counterfactual branches at the
second payment date and buys a different loan, which later defaults. Rates and the loans'
outcomes stay observed (exogenous). From the branch on, the counterfactual deal recomputes its
own collateral, tests, diversions and tranche flows. The real deal's reported distributions may
stand in for the baseline, but are refused for the counterfactual: its states are derived, not
observed. An investor holds the tranches in both worlds.

Run it with: .venv/bin/python examples/e2_clo_counterfactual.py
"""

import datetime as dt

import polars as pl

import fi_engine as fi

months = [dt.date(2023, 1, 31) + dt.timedelta(days=30 * k) for k in range(19)]
cal = fi.Calendar(months)
quarters = months[3::3]
loans = [f"L{j}" for j in range(1, 8)]

ws = fi.Workspace()
ws.series("sofr", pl.DataFrame({"date": months, "rate": [4.5 + 0.05 * k for k in range(19)]}), time="date", values={"rate": "%"})
ws.series("loans", pl.DataFrame({
    "loan": [n for _ in months for n in loans], "date": [m for m in months for _ in loans],
    "clean": [97.0 if n == "L4" and k >= 6 else 99.0 for k, _ in enumerate(months) for n in loans],
    "accrued": [0.0] * (len(months) * len(loans)),
}), entity="loan", kind="loan", time="date", values={"clean": "price", "accrued": "price"}, grid=True)
# Each loan pays 0.6% of face a month; L7 defaults in month 9, recovering 40%.
rows = [(n, m, 0.006, 0.0, 0.0) for n in loans for k, m in enumerate(months[1:], 1) if not (n == "L7" and k >= 9)]
rows.append(("L7", months[9], 0.0, 0.0, 0.6))
ws.series("loan_flows", pl.DataFrame(rows, schema={"loan": pl.String, "date": pl.Date, "interest": pl.Float64, "principal_face": pl.Float64,
                                                   "writedown_face": pl.Float64}, orient="row"),
          entity="loan", kind="loan", time="date", values={"interest": "number", "principal_face": "number", "writedown_face": "number"})
# L4 is downgraded to CCC in month 5, reported three days later.
ws.series("ratings", pl.DataFrame({"loan": ["L4"], "date": [months[5]], "known": [months[5] + dt.timedelta(days=3)], "ccc": [1.0]}),
          entity="loan", kind="loan", time="date", known="known", values={"ccc": "flag"})
# The manager's recorded trades: at the branch, sell L2 and buy L5. The alternative buys L7.
branch = quarters[1]
for name, buy in [("trades", "L5"), ("alternative", "L7")]:
    ws.series(name, pl.DataFrame({"loan": ["L2", buy], "date": [branch] * 2, "qty": [-25.0, 25.0]}), entity="loan", kind="loan", time="date",
              values={"qty": "number"})

def clo(w: fi.Workspace, manager: fi.Decision) -> fi.Model:
    """The deal, in world `w` (its tranches and tests become entities of that world)."""
    pool = w.supplied_bonds("loans", flows="loan_flows")
    return w.deal(
        "clo", cal, collateral=pool, opening={"L1": 25.0, "L2": 25.0, "L3": 25.0, "L4": 25.0},
        tranches=[fi.Tranche("A", 70.0, coupon=fi.floating("sofr.rate", spread_bp=150)),
                  fi.Tranche("B", 15.0, coupon=0.07, shortfall="defer_compound"), fi.Tranche.equity("E", 15.0)],
        fees=[fi.Fee("senior", rate_bp=15.0), fi.Fee("sub", rate_bp=35.0)],
        haircuts=[fi.Haircut("ccc", when="ratings.ccc", carry=("market",), excess_over=0.075)],
        tests=[fi.Test.oc("A_OC", ["A"], 1.25, cure=["A"]), fi.Test.ic("A_IC", ["A"], 1.3, cure=["A"]),
               fi.Test.oc("B_OC", ["A", "B"], 1.10, cure=["A", "B"])],
        interest=["fee:senior", "interest:A", "test:A_OC", "test:A_IC", "interest:B", "test:B_OC", "fee:sub", "residual:E"],
        principal=["sequential:A,B when amortising"],
        payments=quarters, reinvest_until=months[12],
        manager=fi.Policy(manager, schedule="all", lag=0), reads=["trades", "alternative"],
    )


recorded = fi.Decision.trades("trades.qty")
base = ws.world()
print("baseline:", base.evaluate([clo(base, recorded)], cal)["mode"])
cf = ws.world()
alternative = fi.Decision.branch(branch, recorded, fi.Decision.trades("alternative.qty"))
print("counterfactual:", cf.evaluate([clo(cf, alternative)], cal)["mode"])
tests = lambda w: w.frame("clo.tests").filter(pl.col("entity") == "clo.B_OC").select(pl.col("time").dt.date(), "value", "passed", "cure")  # noqa: E731
print("B_OC, baseline then counterfactual:")
print(tests(base).join(tests(cf), on="time", suffix="_cf"))
equity = lambda w: w.frame("clo.flows").filter(pl.col("entity") == "clo.E")["interest"].sum() * 15  # noqa: E731
print(f"equity distributions: baseline {equity(base):.3f}, counterfactual {equity(cf):.3f}")

# The trustee's reported tranche flows: they may stand in for the baseline's, never for the
# counterfactual's.
reported = base.frame("clo.flows").select(pl.col("entity").alias("tranche"), pl.col("time").dt.date().alias("date"), "interest", "principal_face")
for w in (base, cf):
    w.series("trustee", reported, entity="tranche", kind="tranche", time="date", values={"interest": "number", "principal_face": "number"},
             describes="clo.flows")
replay = base.world()
print("replayed from the trustee:", replay.evaluate([clo(replay, recorded)], cal, substitute={"clo.flows": "trustee"})["mode"])
try:
    again = cf.world()
    again.evaluate([clo(again, alternative)], cal, substitute={"clo.flows": "trustee"})
except fi.InvalidError as e:
    print("refused:", e)

# An investor holds A and the equity in both worlds.
for label, w in [("baseline", base), ("counterfactual", cf)]:
    run = w.run(fi.Book([], opening=[("clo.A", 10.0), ("clo.E", 5.0)], initial_cash=0.0), w.records(w.tranches("clo", ["A", "E"]), cal))
    print(f"investor, {label}: {run.mode}; NAV {run.final_nav:.3f}; identity error {run.max_identity_error:.1e}")
