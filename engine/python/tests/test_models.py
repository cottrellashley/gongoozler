"""Models from Python: built in or Python callables, point in time, cached, failing loudly."""

from __future__ import annotations

import datetime as dt

import numpy as np
import polars as pl
import pytest

import fi_engine as fi
from conftest import weekdays


def zscore_by_group(t, entities, x):
    """The built-in z-score by group, in NumPy: what a pandas user would bring."""
    s, g = x
    out = np.full_like(s, np.nan)
    ok = np.isfinite(s) & np.isfinite(g)
    for code in np.unique(g[ok]):
        m = ok & (g == code)
        if m.sum() >= 2 and s[m].std() > 0:
            out[m] = (s[m] - s[m].mean()) / s[m].std()
    return out


def test_a_python_model_matches_the_built_in_one(synthetic):
    ws, u, _ = synthetic
    built_in = fi.Model.zscore("z", "marks.signal", group="sector.code")
    python = fi.cross_section("zp", ["marks.signal", fi.asof("sector.code")], ["z"])(zscore_by_group)
    rep = ws.evaluate([built_in, python], u["calendar"])
    assert sorted(rep["evaluated"]) == ["z", "zp"]
    a = ws.frame("z").sort("entity", "time")["z"].to_numpy()
    b = ws.frame("zp").sort("entity", "time")["z"].to_numpy()
    assert np.array_equal(np.isnan(a), np.isnan(b))
    assert np.allclose(a[~np.isnan(a)], b[~np.isnan(b)], rtol=0, atol=1e-12)


def test_a_python_model_sees_only_what_was_known():
    """Values published two days after their date are invisible to a model with a one-day
    window, and visible with a three-day one."""
    dates = weekdays(dt.date(2024, 1, 1), 10)
    df = pl.DataFrame({"e": ["A", "B"] * 10, "date": [d for d in dates for _ in range(2)], "v": np.arange(20, dtype=float)})
    ws = fi.Workspace()
    ws.series("late", df, entity="e", time="date", known=fi.lag(days=2), values={"v": "number"}, grid=True)
    seen = {}

    def record(name):
        def f(t, entities, x):
            seen.setdefault(name, []).append(x[0].copy())
            return x[0]

        return f

    one = fi.cross_section("one", ["late.v"], ["v"], window_days=1)(record("one"))
    three = fi.cross_section("three", ["late.v"], ["v"], window_days=3)(record("three"))
    ws.evaluate([one, three], ws.calendar("late"))
    assert all(np.isnan(x).all() for x in seen["one"])
    assert np.array_equal(np.concatenate(seen["three"]), np.arange(20, dtype=float))
    # The output's knowledge time is the evaluation time plus the window plus the lag.
    out = ws.frame("three").sort("time")
    assert (out["known"] - out["time"]).unique().to_list() == [dt.timedelta(days=3)]


def test_model_errors_come_back_unchanged_and_leave_nothing_behind(synthetic):
    ws, u, _ = synthetic

    @fi.cross_section("boom", ["marks.signal"], ["x"])
    def boom(t, entities, x):
        if t >= u["calendar"].times()[100]:
            raise ValueError("cannot fit")
        return x[0]

    with pytest.raises(ValueError, match="cannot fit"):
        ws.evaluate([boom], u["calendar"])
    assert "boom" not in ws.names()

    @fi.cross_section("shape", ["marks.signal"], ["x"])
    def shape(t, entities, x):
        return x[0][:-1]

    with pytest.raises(fi.InvalidError, match="wrong shape"):
        ws.evaluate([shape], u["calendar"])
    # A model may not take the name of registered data: every state has one producer.
    with pytest.raises(fi.InvalidError, match="registered series"):
        ws.evaluate([fi.Model.zscore("marks", "marks.signal")], u["calendar"])


def test_evaluations_are_cached_by_their_inputs(synthetic):
    ws, u, _ = synthetic
    vol = fi.Model.ewma_vol("vol", "equity.value", halflife=10)
    calls = []

    @fi.cross_section("py", ["marks.signal"], ["x"])
    def py(t, entities, x):
        calls.append(t)
        return x[0] * 2

    first = ws.evaluate([vol, py], u["calendar"])
    again = ws.evaluate([vol, py], u["calendar"])
    assert sorted(first["evaluated"]) == ["py", "vol"] and first["cached"] == []
    assert again["evaluated"] == [] and sorted(again["cached"]) == ["py", "vol"]
    assert len(calls) == len(u["calendar"]), "the Python model ran once, once per time"


def test_merton_per_issuer_reaches_bonds_through_a_dated_relationship():
    """Frames as a user brings them: equity per issuer, filed debt, a market rate, and a bond →
    issuer map. Merton runs per issuer; a bond-level signal reads it through the map."""
    rng = np.random.default_rng(5)
    dates = weekdays(dt.date(2024, 1, 1), 120)
    issuers = ["ACME", "BETA", "COBALT"]
    equity = pl.DataFrame({
        "issuer": np.tile(issuers, len(dates)),
        "date": [d for d in dates for _ in range(3)],
        "equity": (np.array([100.0, 50.0, 20.0]) * np.exp(np.cumsum(rng.normal(0, 0.02, (len(dates), 3)), axis=0))).ravel(),
    })
    debt = pl.DataFrame({"issuer": issuers, "period_end": [dates[0]] * 3, "filed": [dates[0]] * 3, "debt": [80.0, 60.0, 30.0]})
    rate = pl.DataFrame({"date": dates, "sofr": [5.0] * len(dates)})
    bonds = pl.DataFrame({"isin": ["B1", "B2", "B3", "B4"], "issuer": ["ACME", "ACME", "BETA", "COBALT"]})

    ws = fi.Workspace()
    ws.series("equity", equity, entity="issuer", time="date", values={"equity": "number"}, grid=True)
    ws.series("financials", debt, entity="issuer", time="period_end", known="filed", values={"debt": "number"})
    ws.series("rate", rate, time="date", values={"sofr": "%"})
    ws.series("issuer", bonds, entity="isin", kind="bond", values={"issuer": fi.ref("issuer")})
    cal = ws.calendar("equity")
    merton = fi.Model.merton("merton", "equity.equity", "financials.debt", "rate.sofr", window=40)
    credit = fi.Model.zscore("credit_z", fi.exact("merton.spread").via("issuer.issuer"))
    ws.evaluate([credit, merton], cal)  # in any order: the graph finds the dependency

    m = ws.frame("merton").filter(pl.col("spread").is_not_null())
    assert m["entity"].unique().sort().to_list() == issuers and (m["pd"] > 0).all() and (m["pd"] < 1).all()
    z = ws.frame("credit_z").filter(pl.col("z").is_not_null())
    last = z["time"].max()
    spread = dict(m.filter(pl.col("time") == last).select("entity", "spread").iter_rows())
    s = np.array([spread[i] for i in ["ACME", "ACME", "BETA", "COBALT"]])
    expected = (s - s.mean()) / s.std()
    got = z.filter(pl.col("time") == last).sort("entity")["z"].to_numpy()
    assert np.allclose(got, expected, atol=1e-12)


def test_a_fair_value_curve_fits_each_date():
    dates = weekdays(dt.date(2024, 1, 1), 5)
    mats = [dt.date(2024 + y, 6, 30) for y in range(1, 11)]
    tau = lambda d, m: ((m - d).days) / 365.25  # noqa: E731
    lam = 1.8
    rows = []
    for d in dates:
        for j, m in enumerate(mats):
            x = tau(d, m) / lam
            f1 = (1 - np.exp(-x)) / x
            y = 0.04 - 0.01 * f1 + 0.02 * (f1 - np.exp(-x)) + (0.0005 if j == 3 else 0.0)
            rows.append((f"G{j}", d, y, (m - dt.date(1970, 1, 1)).days))
    df = pl.DataFrame(rows, schema=["govt", "date", "yield", "maturity"], orient="row")
    ws = fi.Workspace()
    ws.series("govt", df, entity="govt", time="date", values={"yield": "rate", "maturity": "number"}, grid=True)
    ws.evaluate([fi.Model.nelson_siegel("fv", "govt.yield", "govt.maturity", lam=lam)], ws.calendar("govt"))
    out = ws.frame("fv").sort("time", "entity")
    rich_cheap = out.group_by("entity").agg(pl.col("residual").mean()).sort("residual")
    assert rich_cheap["entity"][-1] == "G3", "the bond priced 5bp cheap is the cheapest"
    assert out["residual"].abs().max() < 0.0005


def test_models_declare_roles_and_are_used_only_once_available(synthetic):
    """A model's role and its producer are recorded on its output. A model that became available
    on a date has no output before it in a point-in-time run, unless the run is retrospective.
    Versions of a model each serve from their own date."""
    ws, u, _ = synthetic
    times = u["calendar"].times()
    late = fi.Model.zscore("late", "marks.signal").role("pricing").available_from(times[100])
    retro = fi.Model.zscore("retro", "marks.signal").available_from(times[100]).retrospective()
    versions = fi.Model.vintaged("versions", [(times[0], fi.Model.zscore("v1", "marks.signal")),
                                              (times[150], fi.Model.zscore("v2", "marks.signal", ddof=1.0))])
    ws.evaluate([late, retro, versions, fi.Model.zscore("v1_only", "marks.signal"), fi.Model.zscore("v2_only", "marks.signal", ddof=1.0)],
                u["calendar"])
    assert ws.provenance("late") == {"knowledge": "derived", "producer": "model late", "role": "pricing", "describes": "late", "intervention": None, "revisions": False}
    frame = lambda name: ws.frame(name).sort("time", "entity")  # noqa: E731
    before = frame("late").filter(pl.col("time") < pl.from_epoch(pl.lit(times[100]), time_unit="ns"))
    assert before["z"].null_count() == before.height, "no output before the model existed"
    assert frame("retro")["z"].null_count() < frame("late")["z"].null_count()
    v, v1, v2 = frame("versions")["z"].to_numpy(), frame("v1_only")["z"].to_numpy(), frame("v2_only")["z"].to_numpy()
    n = len(ws.entities("marks"))
    assert np.allclose(v[: 150 * n], v1[: 150 * n], equal_nan=True) and np.allclose(v[150 * n:], v2[150 * n:], equal_nan=True)


def ns_basis(tau, lam):
    x = tau / lam
    f1 = (1 - np.exp(-x)) / x
    return np.column_stack([np.ones_like(tau), f1, f1 - np.exp(-x)])


def test_a_fair_value_curve_in_python_matches_the_built_in_one(synthetic):
    """The same Nelson-Siegel fit, by NumPy's least squares in a Python model."""
    ws, u, _ = synthetic

    @fi.cross_section("fv_py", ["govt.yield", fi.asof("govt.maturity")], ["fitted", "residual"])
    def fv_py(t, entities, x):
        y, maturity = x
        tau = (maturity - t // fi.DAY) / 365.25
        fitted, residual = np.full_like(y, np.nan), np.full_like(y, np.nan)
        use = np.isfinite(y) & (tau > 0)
        if use.sum() >= 5:
            beta = np.linalg.lstsq(ns_basis(tau[use], 1.8), y[use], rcond=None)[0]
            pos = tau > 0
            fitted[pos] = ns_basis(tau[pos], 1.8) @ beta
            residual[pos] = y[pos] - fitted[pos]
        return fitted, residual

    ws.evaluate([fi.Model.nelson_siegel("fv", "govt.yield", "govt.maturity", lam=1.8, min_points=5), fv_py], u["calendar"])
    a, b = ws.frame("fv").sort("entity", "time"), ws.frame("fv_py").sort("entity", "time")
    assert a["fitted"].null_count() == b["fitted"].null_count() < a.height
    for c in ("fitted", "residual"):
        assert np.allclose(a[c].to_numpy(), b[c].to_numpy(), rtol=0, atol=1e-12, equal_nan=True)
