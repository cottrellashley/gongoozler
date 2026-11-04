# /// script
# requires-python = ">=3.13"
# dependencies = ["fi-engine", "numpy", "polars"]
# [tool.uv.sources]
# fi-engine = { path = "..", editable = true }
# [tool.uv]
# exclude-newer = "2026-11-01T00:00:00Z"
# ///
"""E3: a forward joint Monte Carlo (the brief's example 3, in Python).

Each world is a copy of one base workspace in which stochastic models write:
- a short-rate path;
- correlated issuer defaults, keyed by borrower, so one borrower's loan in the CLO and its bond
  default together, each recovering under its own terms.

In every world, before any average:
- the CLO's collateral and waterfall run;
- a pricing model marks the bond from that world's rate, as known then;
- a book financed by a repo, with performance-chasing redemptions, is run under two policies on
  the same draws.

Real-world (P) models generate the world; the pricing model (Q) values in it. Each is reported in
its own role, and the run's mode is "simulation".

Run it with: .venv/bin/python examples/e3_forward_monte_carlo.py
"""

import datetime as dt

import numpy as np
import polars as pl

import fi_engine as fi

WORLDS = 40
months = [dt.date(2025, 1, 31) + dt.timedelta(days=30 * k) for k in range(37)]
cal = fi.Calendar(months)
quarters = months[3::3]
borrowers = ["A", "B", "C", "D", "E", "F"]
loans = [f"L{b}" for b in borrowers]

base = fi.Workspace()
# What does not vary by world is registered once and shared by every world: marks at par for the
# loans (a valuation convention), and the bond's terms.
base.series("loans", pl.DataFrame({"loan": [n for _ in months for n in loans], "date": [m for m in months for _ in loans],
                                   "clean": [100.0] * (len(months) * len(loans)), "accrued": [0.0] * (len(months) * len(loans))}),
            entity="loan", kind="loan", time="date", values={"clean": "price", "accrued": "price"}, grid=True)
base.series("bond_terms", pl.DataFrame({"loan": ["BXA"], "duration": [4.0]}), entity="loan", kind="loan", values={"duration": "number"})


@fi.cross_section("bond_marks", [fi.asof("bond_terms.duration"), fi.market("rate.value")], ["clean", "accrued"], universe="bond_terms")
def price_bond(t, entities, x):
    """Q: the bond's clean price from the world's rate as known at t, by duration."""
    duration, rate = x
    return 100.0 - duration * (rate - 0.04) * 100.0, np.zeros_like(rate)


pricing = price_bond.role("pricing")


def world(w: int) -> fi.Workspace:
    ws = base.world()
    rates = ws.gaussian_paths("rate", 0.04, mean=0.035, reversion=0.4, vol=0.008, floor=0.0, seed=7, world=w)
    terms = [(f"L{b}", b, 0.075, months[-1], 0.04, 0.45) for b in borrowers] + [("BXA", "A", 0.06, months[-1], 0.04, 0.65)]
    defaults = ws.defaults("flows", terms, months[1:], correlation=0.35, seed=7, world=w)
    # The world first: the deal's collateral is contracts on the flows it writes.
    ws.evaluate([rates, defaults], cal)
    pool = ws.supplied_bonds("loans", flows="flows", entities=loans)
    clo = ws.deal(
        "clo", cal, collateral=pool, opening=dict.fromkeys(loans, 100.0 / 6),
        tranches=[fi.Tranche("A", 70.0, coupon=fi.floating("rate.value", spread_bp=140)), fi.Tranche.equity("E", 30.0)],
        tests=[fi.Test.oc("A_OC", ["A"], 1.2, cure=["A"])],
        interest=["interest:A", "test:A_OC", "residual:E"], principal=["sequential:A"], payments=quarters,
    )
    report = ws.evaluate([rates, defaults, clo, pricing], cal)  # the world's models come from the cache
    assert report["mode"] == "simulation"
    return ws


def redemptions(ctx):
    """Investors redeem a tenth of the fund after a quarter in which it lost 2% or more."""
    npu = ctx["nav_per_unit"]
    return -0.1 * ctx["nav"][-1] if len(npu) > 3 and npu[-1] < 0.98 * npu[-4] else 0.0


@fi.decision
def derisk(ctx):
    """Sell half the bond after the first year."""
    return fi.orders({"BXA": -25.0}) if ctx.k == 12 else None


# The repo is repaid from free cash at each step: the sale's proceeds retire it.
repo = fi.Agreement.secured("repo", {"BXA": 0.8}, rate="rate.value", opening=20.0, cure=1, sweep=True)


def book(policies) -> fi.Book:
    return fi.Book(policies, opening=[("clo.A", 30.0), ("clo.E", 10.0), ("BXA", 50.0)], initial_cash=5.0,
                   financing=[repo], responses=["convert", "draw", ("sell", 100.0)], flows=fi.flows.model(redemptions))


hold, sell = book([]), book([fi.Policy(derisk, schedule="all", lag=0)])
results = []
for w in range(WORLDS):
    ws = world(w)
    contracts = ws.tranches("clo", ["A", "E"]) + ws.supplied_bonds("bond_marks", flows="flows", entities=["BXA"])
    records = ws.records(contracts, cal)
    a, b = ws.sweep([hold, sell], records)
    loss = ws.frame("clo.flows").filter(pl.col("entity") == "clo.E")["writedown_face"].sum()
    defaulted = ws.frame("flows").filter(pl.col("defaulted") == 1.0)["entity"].n_unique()
    results.append((a.ledger()["nav_per_unit"][-1], b.ledger()["nav_per_unit"][-1], a.failed_at is not None, b.failed_at is not None, defaulted, a.mode))

r = pl.DataFrame(results, schema=["hold", "derisk", "hold_failed", "derisk_failed", "defaults", "mode"], orient="row")
print(f"{WORLDS} worlds, mode {r['mode'][0]}; mean defaults per world {r['defaults'].mean():.2f}")
for policy in ["hold", "derisk"]:
    v = r[policy]
    print(f"{policy:>7}: NAV per unit mean {v.mean():.2f}, 5th percentile {v.quantile(0.05):.2f}, "
          f"failed in {r[policy + '_failed'].sum()} worlds")
print(f"de-risking beats holding in {(r['derisk'] > r['hold']).sum()} of {WORLDS} worlds (the same draws for both)")
for name in ["rate", "flows", "clo.flows", "bond_marks"]:
    p = ws.provenance(name)
    print(f"{name:>10}: produced by {p['producer']}{', ' + p['role'] if 'role' in p else ''}; {p['intervention'][0]} by {p['intervention'][1]}")
