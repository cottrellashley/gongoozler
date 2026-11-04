"""Feeding data: any Arrow frame, declared as a series, checked when it arrives."""

from __future__ import annotations

import datetime as dt

import numpy as np
import pandas as pd
import polars as pl
import pyarrow as pa
import pytest

import fi_engine as fi
from conftest import marks

DECL = dict(entity="isin", kind="bond", time="date", values={"clean": "price", "accrued": "price"})


def test_polars_pandas_and_pyarrow_frames_make_the_same_series():
    df = marks(bonds=5, days=10)
    versions = set()
    for data in (df, df.to_pandas(), df.to_arrow(), pa.table(df.to_pandas())):
        ws = fi.Workspace()
        versions.add(ws.series("marks", data, **DECL)["version"])
    assert len(versions) == 1


def test_only_the_declared_columns_are_read():
    df = marks(bonds=3, days=4).with_columns(
        pl.col("isin").cast(pl.Categorical).alias("cat"),
        pl.lit(None, dtype=pl.List(pl.Int64)).alias("nested"),
        pl.col("clean").cast(pl.Decimal(10, 4)).alias("clean_dec"),
    )
    ws = fi.Workspace()
    info = ws.series("marks", df, **DECL)
    assert info["rows"] == 12 and info["entities"] == 3
    # A categorical entity column and a decimal value column cross as text and floats.
    ws.series("m2", df, entity="cat", kind="bond", time="date", values={"clean_dec": "price"})
    a = ws.frame("marks").sort("entity", "time")["clean"].to_numpy()
    b = ws.frame("m2").sort("entity", "time")["clean_dec"].to_numpy()
    assert np.allclose(a, b, atol=1e-4)


def test_units_are_declared_never_guessed():
    df = pl.DataFrame({"isin": ["A", "B"], "date": [dt.date(2024, 1, 2)] * 2, "y_pct": [4.5, 5.0], "oas_bp": [120.0, 85.0]})
    ws = fi.Workspace()
    ws.series("rates", df, entity="isin", time="date", values={"y_pct": "%", "oas_bp": "bp"})
    out = ws.frame("rates").sort("entity")
    assert out["y_pct"].to_list() == pytest.approx([0.045, 0.05])
    assert out["oas_bp"].to_list() == pytest.approx([0.012, 0.0085])


def test_bad_frames_fail_when_they_arrive():
    df = marks(bonds=2, days=3)
    ws = fi.Workspace()
    with pytest.raises(fi.UnknownError, match="no column nope"):
        ws.series("x", df, entity="isin", time="date", values={"nope": "price"})
    nan = df.with_columns(pl.when(pl.int_range(pl.len()) == 2).then(float("nan")).otherwise(pl.col("clean")).alias("clean"))
    with pytest.raises(fi.InvalidError, match="NaN at row 2"):
        ws.series("x", nan, **DECL)
    ws.series("x", nan, **DECL, nan_is_missing=True)  # pandas producers mean "missing" by NaN
    with pytest.raises(fi.InvalidError, match="entity is null"):
        ws.series("y", df.with_columns(pl.lit(None, dtype=pl.String).alias("isin")), **DECL)
    with pytest.raises(fi.InvalidError):
        ws.series("z", pl.concat([df, df]), **DECL)  # two rows for one entity and time
    with pytest.raises(fi.InvalidError, match="infinite"):
        ws.series("w", df.with_columns(pl.lit(float("inf")).alias("clean")), **DECL)


def test_knowledge_times_from_a_column_a_lag_or_rows():
    d = [dt.date(2024, 1, 1) + dt.timedelta(days=i) for i in range(4)]
    df = pl.DataFrame({"e": ["A"] * 4, "date": d, "pub": [dt.datetime(2024, 1, 3)] * 4, "v": [1.0, 2.0, 3.0, 4.0]})
    ws = fi.Workspace()
    ws.series("col", df, entity="e", time="date", known="pub", values={"v": "number"})
    ws.series("lag", df, entity="e", time="date", known=fi.lag(days=1, hours=6), values={"v": "number"})
    ws.series("rows", df, entity="e", time="date", known=fi.lag(rows=1), values={"v": "number"})
    day = fi.DAY
    t0 = (d[0] - dt.date(1970, 1, 1)).days * day
    known = lambda name: [k.timestamp() for k in ws.frame(name).sort("time")["known"].dt.replace_time_zone("UTC").to_list()]  # noqa: E731
    assert known("col") == [dt.datetime(2024, 1, 3, tzinfo=dt.UTC).timestamp()] * 4
    assert known("lag")[0] == (t0 + day + day // 4) / 1e9
    rows = ws.frame("rows").sort("time")["known"].to_list()
    assert rows[:3] == [dt.datetime(2024, 1, 2), dt.datetime(2024, 1, 3), dt.datetime(2024, 1, 4)]  # the next row's date
    assert rows[3] > dt.datetime(2200, 1, 1), "the last row is never known"


def test_relationships_and_categories_arrive_as_text_and_leave_as_names():
    df = pl.DataFrame({"isin": ["B1", "B2", "B3"], "issuer": ["ACME", "ACME", None], "sector": ["fin", "ind", "fin"]})
    ws = fi.Workspace()
    ws.series("static", df, entity="isin", kind="bond", values={"issuer": fi.ref("issuer"), "sector": "category"})
    out = ws.frame("static").sort("entity")
    assert out["issuer"].to_list() == ["ACME", "ACME", None]
    assert out["sector"].to_list() == ["fin", "ind", "fin"]


def test_pandas_wide_panels_register_as_they_are():
    dates = pd.bdate_range("2024-01-01", periods=5, name="date")
    wide = pd.DataFrame({"US1": [1.0, 2.0, np.nan, 4.0, 5.0], "US2": [10.0, 20.0, 30.0, 40.0, 50.0]}, index=dates)
    ws = fi.Workspace()
    info = ws.series_wide("tr", wide, time="date", kind="bond", column="level")
    assert info["layout"] == "grid" and info["entities"] == 2
    out = ws.frame("tr").sort("entity", "time")
    assert out.filter(pl.col("entity") == "US1")["level"].to_list() == [1.0, 2.0, None, 4.0, 5.0]


def test_times_come_from_any_python_time():
    ns = 1_704_153_600_000_000_000  # 2024-01-02 00:00 UTC
    for t in (ns, dt.date(2024, 1, 2), dt.datetime(2024, 1, 2), dt.datetime(2024, 1, 2, 1, tzinfo=dt.timezone(dt.timedelta(hours=1))),
              "2024-01-02", np.datetime64("2024-01-02"), pd.Timestamp("2024-01-02")):
        assert fi.Calendar([t]).times() == [ns], t
    cal = fi.Calendar.weekdays("2024-01-01", "2024-01-31")
    assert len(cal) == 23 and cal.dates()[-1] == dt.date(2024, 1, 31)
    assert [dt.datetime.fromtimestamp(t / 1e9, dt.UTC).date() for t in cal.month_ends()] == [dt.date(2024, 1, 31)]
