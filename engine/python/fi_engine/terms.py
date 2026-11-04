"""Debt from its terms: where its prices come from, and bonds from a terms frame.

A desk keeps its universe as a terms table, one row per instrument; ``bonds`` turns one into
``Bond`` objects for ``Workspace.debt``. Every convention is a column the table declares; nothing is
assumed about a market.
"""

from __future__ import annotations

from collections.abc import Mapping
from typing import Any

from ._native import Bond, BusinessCalendar, Coupon


class valuation:  # noqa: N801 - a namespace
    """Where a debt contract's clean price comes from."""

    @staticmethod
    def clean(spec: str) -> tuple:
        """Observed clean prices per 100 of current face: ``"series.column"``, keyed by instrument."""
        return ("clean", spec)

    @staticmethod
    def yield_(spec: str, *, convention: str = "street") -> tuple:
        """Observed yields (decimals), ``"series.column"`` keyed by instrument, priced through the
        terms: ``"street"`` compounds at the coupon frequency, ``"annual"`` and ``"continuous"``
        over ACT/365F years."""
        return ("yield", spec, convention)

    @staticmethod
    def curve(
        series: str,
        name: str,
        points: Mapping[str, float],
        *,
        compounding: str = "continuous",
        interpolation: str = "linear_zero",
        spread: float | str | None = None,
    ) -> tuple:
        """Prices from a curve observed as zero rates: ``series`` is keyed by curve and ``name`` is
        this one; ``points`` maps each column to its tenor in years. ``spread`` over it: a constant
        (a continuously compounded decimal) or ``"series.column"`` observed per instrument."""
        pts = sorted(((c, float(t)) for c, t in points.items()), key=lambda p: p[1])
        return ("curve", series, name, pts, compounding, interpolation, spread)


def _coupon(row: Mapping[str, Any]) -> Any:
    kind = row.get("coupon_type")
    if kind is None:
        if row.get("real") is not None:
            kind = "inflation"
        elif row.get("index") is not None:
            kind = "floating"
        elif row.get("coupon") is None:
            kind = "zero"
        else:
            kind = "fixed"
    if kind == "fixed":
        return Coupon.fixed(float(row["coupon"]))
    if kind == "zero":
        return None
    if kind == "floating":
        opts = {k: row[k] for k in ("spread", "gearing", "cap", "floor", "method", "lookback", "lockout") if row.get(k) is not None}
        if row.get("fixing_lag") is not None:
            opts["lag"] = int(row["fixing_lag"])
        return Coupon.floating(row["index"], **opts)
    if kind == "inflation":
        opts = {k: row[k] for k in ("interpolate",) if row.get(k) is not None}
        if row.get("index_lag") is not None:
            opts["lag"] = int(row["index_lag"])
        if row.get("deflation_floor") is not None:
            opts["floor"] = bool(row["deflation_floor"])
        return Coupon.inflation(row["index"], float(row["real"]), float(row["base_index"]), **opts)
    raise ValueError(f"{row.get('name')}: no coupon type {kind!r} (fixed, floating, inflation, zero)")


_SCHEDULE = ("frequency", "business_day", "adjust_accrual", "eom", "stub", "pay_lag", "ex_days", "settle_days", "redemption", "currency", "rounding")


def bonds(
    terms: Any,
    *,
    name: str = "name",
    calendars: Mapping[str, BusinessCalendar] | None = None,
    rights: Any = None,
    amortisation: Any = None,
) -> dict[str, Bond]:
    """``Bond`` objects from a terms frame (Polars, pandas, pyarrow), one row per instrument.

    Columns: ``name`` (the instrument), ``issue``, ``maturity`` and ``day_count``; optionally
    ``coupon`` (a decimal rate), ``frequency``, ``currency``, ``calendar`` (a key of
    ``calendars``), ``business_day``, ``adjust_accrual``, ``eom``, ``stub``, ``first``, ``last``,
    ``pay_lag``, ``ex_days``, ``settle_days``, ``redemption``, ``rounding``, ``extension`` (a date). A floater has
    ``index`` with ``spread``, ``gearing``, ``cap``, ``floor``, ``method``, ``fixing_lag``,
    ``lookback`` and ``lockout``; a linker has ``index``, ``real``, ``base_index``, ``index_lag``,
    ``interpolate`` and ``deflation_floor``. ``coupon_type`` says which, where it is ambiguous.

    ``rights`` is a frame of calls and puts (``name``, ``kind``: call or put, ``from``, ``to``,
    ``price``, optionally ``notice`` and ``make_whole_bp``); ``amortisation`` one of principal
    repayments (``name``, ``date``, ``face`` per unit of original face, ``price`` per 100).
    """
    from . import _polars

    cals = dict(calendars or {})
    by_name: dict[str, dict[str, list]] = {}
    if rights is not None:
        for r in _polars(rights).iter_rows(named=True):
            entry = (r["from"], r["to"], float(r["price"]), int(r.get("notice") or 0), r.get("make_whole_bp"))
            kind = (r.get("kind") or "call").lower()
            if kind not in ("call", "put"):
                raise ValueError(f"{r[name]}: a right is a call or a put, not {kind!r}")
            by_name.setdefault(r[name], {}).setdefault(f"{kind}s", []).append(entry)
    if amortisation is not None:
        for r in _polars(amortisation).iter_rows(named=True):
            by_name.setdefault(r[name], {}).setdefault("amortisation", []).append((r["date"], float(r["face"]), float(r.get("price") or 100.0)))
    out: dict[str, Bond] = {}
    for row in _polars(terms).iter_rows(named=True):
        key = row[name]
        if key in out:
            raise ValueError(f"{key}: two rows of terms")
        opts: dict[str, Any] = {k: row[k] for k in _SCHEDULE if row.get(k) is not None}
        for k in ("first", "last"):
            if row.get(k) is not None:
                opts[k] = row[k]
        if row.get("calendar") is not None:
            if row["calendar"] not in cals:
                raise ValueError(f"{key}: no calendar {row['calendar']!r} among {sorted(cals)}")
            opts["calendar"] = cals[row["calendar"]]
        if row.get("extension") is not None:
            opts["extension"] = (row["extension"],)
        opts.update(by_name.get(key, {}))
        out[key] = Bond(row["issue"], row["maturity"], _coupon({**row, "name": key}), day_count=row["day_count"], **opts)
    return out
