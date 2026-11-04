"""Frames a user would bring: built here in Polars and NumPy, as they would come from anywhere."""

from __future__ import annotations

import datetime as dt

import numpy as np
import polars as pl
import pytest

import fi_engine as fi


def weekdays(start: dt.date, n: int) -> list[dt.date]:
    out, d = [], start
    while len(out) < n:
        if d.weekday() < 5:
            out.append(d)
        d += dt.timedelta(days=1)
    return out


def marks(bonds: int = 20, days: int = 60, seed: int = 1) -> pl.DataFrame:
    """Dense daily marks: clean (per 100), accrued (per 100) and a signal, for every bond and day."""
    rng = np.random.default_rng(seed)
    dates = weekdays(dt.date(2024, 1, 1), days)
    ids = [f"US{j:04d}" for j in range(bonds)]
    clean = 100 + np.cumsum(rng.normal(0, 0.2, (days, bonds)), axis=0)
    accrued = np.tile((np.arange(days) % 30)[:, None] * 0.01, (1, bonds))
    return pl.DataFrame(
        {
            "isin": np.tile(ids, days),
            "date": np.repeat(np.array(dates, dtype="datetime64[D]"), bonds),
            "clean": clean.ravel(),
            "accrued": accrued.ravel(),
            "signal": rng.normal(0, 1, days * bonds),
        }
    ).with_columns(pl.col("date").cast(pl.Date))


def level(days: int = 60, seed: int = 2) -> pl.DataFrame:
    rng = np.random.default_rng(seed)
    dates = weekdays(dt.date(2024, 1, 1), days)
    return pl.DataFrame({"index": ["IDX"] * days, "date": dates, "level": 100 * np.exp(np.cumsum(rng.normal(0, 0.01, days)))})


@pytest.fixture
def bonds() -> tuple[fi.Workspace, fi.Records, pl.DataFrame]:
    """A workspace with marks for 20 bonds over 60 weekdays, and their unit records."""
    ws = fi.Workspace()
    df = marks()
    ws.series("marks", df, entity="isin", kind="bond", time="date", values={"clean": "price", "accrued": "price", "signal": "number"}, grid=True)
    rec = ws.records(ws.supplied_bonds("marks"), ws.calendar("marks"))
    return ws, rec, df


@pytest.fixture
def synthetic() -> tuple[fi.Workspace, dict, fi.Records]:
    """The synthetic universe the Rust tests use: 60 bonds × 300 days, with paydowns and PIK."""
    ws = fi.Workspace()
    u = ws.synthetic(bonds=60, days=300, issuers=6, govts=12)
    rec = ws.records(u["bonds"], u["calendar"])
    return ws, u, rec
