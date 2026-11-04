"""fi_engine: a generic backtesting engine for fixed income.

The engine is Rust (``crates/``); this package configures it from Python. The engine never reads
data: frames come from anywhere (Polars, pandas, pyarrow, DuckDB, a Snowflake cursor's Arrow
batches) and each is declared as a series, which is the contract it is checked against. Results
come back as Polars frames.

    import fi_engine as fi

    ws = fi.Workspace()
    ws.series("marks", marks, entity="isin", kind="bond", time="date", known=fi.lag(hours=18),
              values={"clean": "price", "accrued": "price", "signal": "number"}, grid=True)
    records = ws.records(ws.supplied_bonds("marks"), ws.calendar("marks"))
    run = ws.run(fi.Strategy(fi.Decision.top_n("marks.signal", 50)), records)
    run.ledger()        # NAV, cash, income, price, principal, other, financing, costs, turnover

Or from terms, so coupons, accrued interest, calls and redemptions come from the instrument:

    ust = fi.Bond("2024-02-15", "2034-02-15", 0.04, day_count="ACT/ACT ICMA", frequency=2)
    contracts = ws.debt({"UST 4 02/34": ust}, fi.valuation.clean("marks.clean"))

The objects are the core's (docs/spec.md): series, entities, models and a graph; contracts and
unit records; the ledger. Python models and decisions are plain callables (``cross_section``,
``decision``); everything else runs in Rust with the GIL released.
"""

from __future__ import annotations

from collections.abc import Callable, Iterable
from typing import Any

from ._native import (
    DAY,
    Agreement,
    Bond,
    Book,
    BusinessCalendar,
    Calendar,
    Constraint,
    Contracts,
    Coupon,
    Curve,
    Decision,
    DecisionContext,
    EngineError,
    Execution,
    Fill,
    Input,
    InvalidError,
    Manifest,
    MissingError,
    Model,
    Policy,
    Records,
    Run,
    UnknownError,
    __version__,
)
from ._native import Workspace as _Workspace
from .deals import Exercise, Fee, Floating, Haircut, Right, Test, Tranche, Trigger, deficiency_ledger, floating, write_down
from .terms import bonds, valuation

__all__ = [
    "DAY",
    "Agreement",
    "Bond",
    "Book",
    "BusinessCalendar",
    "Calendar",
    "Constraint",
    "Contracts",
    "Coupon",
    "Curve",
    "Decision",
    "DecisionContext",
    "EngineError",
    "Execution",
    "Exercise",
    "Fee",
    "Fill",
    "Floating",
    "Haircut",
    "Input",
    "InvalidError",
    "Manifest",
    "MissingError",
    "Model",
    "Policy",
    "Records",
    "Right",
    "Run",
    "Strategy",
    "Test",
    "Tranche",
    "Trigger",
    "UnknownError",
    "Workspace",
    "__version__",
    "asof",
    "bonds",
    "cross_section",
    "decision",
    "deficiency_ledger",
    "exact",
    "floating",
    "flows",
    "lag",
    "market",
    "orders",
    "quantities",
    "ref",
    "transition",
    "valuation",
    "weights",
    "write_down",
]

exact = Input.exact
asof = Input.asof
market = Input.market


def lag(*, days: float = 0, hours: float = 0, minutes: float = 0, seconds: float = 0, rows: int | None = None) -> tuple[str, int]:
    """When a series' values became known: a fixed time after the event time, or (``rows=n``)
    the event time of the n-th later row of the same entity, as legacy frameworks count lags."""
    if rows is not None:
        if days or hours or minutes or seconds:
            raise ValueError("a lag is in time or in rows, not both")
        return ("rows", int(rows))
    return ("lag", round((((days * 24 + hours) * 60 + minutes) * 60 + seconds) * 1e9))


def transition(
    at: Any, from_: str, into: dict[str, float] | None = None, *, cash: float = 0.0, role: str = "principal", note: str = ""
) -> tuple:
    """A holding transition, for ``Workspace.records(..., transitions=[...])``: at ``at``, each unit
    of ``from_`` becomes ``into`` units of other contracts and ``cash`` (per unit, in its
    currency), for every holder at once, at the values of both at that time. The difference is
    booked once, as ``role``'s term: ``"principal"`` (an exchange or a conversion), ``"recovery"``
    (settling a defaulted claim), ``"exchange"``, ``"income"``, ``"price"`` or ``"fee"``. Orders
    still working in ``from_`` are cancelled; undelivered units follow the new claim."""
    return (at, from_, dict(into or {}), float(cash), role, note)


def ref(kind: str) -> tuple[str, str]:
    """A relationship column: each value names an entity of ``kind`` (a bond's issuer)."""
    return ("ref", kind)


def weights(targets: Any) -> tuple[str, Any]:
    """Decision targets as weights of NAV (a mapping of contract name to weight)."""
    return ("weights", targets)


def quantities(targets: Any) -> tuple[str, Any]:
    """Decision targets as quantities in units (a mapping of contract name to units)."""
    return ("quantities", targets)


def orders(deltas: Any) -> tuple[str, Any]:
    """Decision intentions as orders: units to buy (positive) or sell, on top of the book. Nothing
    else is sold."""
    return ("orders", deltas)


class flows:  # noqa: N801 - a namespace
    """Subscriptions and redemptions for a book: unitised, so returns are time-weighted."""

    @staticmethod
    def series(spec: str, *, settle: int = 0) -> tuple[str, str, int]:
        """Observed flows: a market-wide `"series.column"` of amounts in the base currency
        (positive subscriptions, negative redemptions), paid `settle` steps after they are
        known."""
        return ("series", spec, settle)

    @staticmethod
    def model(f: Callable[[dict], float], *, settle: int = 0) -> tuple[str, Callable, int]:
        """Modelled flows: `f(ctx)` returns the flow now from the book's own history up to the
        previous time (`ctx["nav"]`, `ctx["nav_per_unit"]`): redemptions that follow
        performance."""
        return ("model", f, settle)


def Strategy(  # noqa: N802 - kept as a constructor-like shortcut
    decide: Decision,
    *,
    schedule: Any = None,
    fill: Any = None,
    holding: str = "drift",
    execution_lag: int = 1,
    initial_cash: float = 1e8,
    name: str | None = None,
    **book: Any,
) -> Book:
    """One policy over the whole book: `decide` on `schedule`, executing `execution_lag` steps
    later through `fill` (a `Fill` or an `Execution`). Any other `Book` setting can be passed."""
    policy = Policy(decide, schedule=schedule, lag=execution_lag, name=name)
    return Book([policy], execution=fill, holding=holding, initial_cash=initial_cash, name=name, **book)


def _polars(data: Any):
    import polars as pl

    if isinstance(data, pl.DataFrame):
        return data
    if type(data).__module__.split(".")[0] == "pandas":
        if type(data.index).__name__ != "RangeIndex":
            data = data.reset_index()
        return pl.from_pandas(data)
    return pl.DataFrame(data)


class Workspace(_Workspace):
    """Series, entities and the models' cached outputs.

    ``Workspace(information="as_known", outcomes="as_booked")``: models and decisions see each
    revised value from its publication (``"first_release"``: corrections never seen;
    ``"legacy_latest"``: revised values dated at first publication, as most legacy frameworks
    read them, not point in time). Contracts step on the vintage that was booked (``"restated"``:
    on the corrected one).
    """

    def series_wide(
        self,
        name: str,
        data: Any,
        *,
        time: str,
        kind: str,
        column: str = "value",
        unit: Any = "number",
        known: Any = None,
        nan_is_missing: bool = True,
    ) -> dict:
        """Registers a wide frame (one row per time, one column per entity), as pandas users
        keep panels. Missing cells are missing values. Pandas frames may carry the time as their
        index."""
        df = _polars(data)
        long = df.unpivot(index=time, variable_name="entity", value_name=column).drop_nulls(column)
        return self.series(
            name, long, entity="entity", kind=kind, time=time, known=known, values={column: unit}, grid=True, nan_is_missing=nan_is_missing
        )

    def world(self) -> Workspace:
        """A world: a copy that shares every series and cached output with this workspace by
        reference (spec, change 6). Evaluate what does not vary by world here first, then each
        world's own stochastic models in its copy."""
        w = Workspace(self.information, self.outcomes)
        self._share_into(w)
        return w

    def tranches(self, deal: str, names: Iterable[str] | None = None, *, marks: str | None = None, currency: str | None = None) -> Contracts:
        """A deal's tranches as contracts, once the deal is evaluated: bonds on its flows, marked
        by the deal (debt at par, equity at book value) or by another ``marks`` series of the
        same tranches (a pricing model's or observed prices). ``names`` are the tranches' short
        names (default: all)."""
        entities = None if names is None else [f"{deal}.{n}" for n in names]
        return self.supplied_bonds(
            marks or f"{deal}.marks", flows=f"{deal}.flows", factor="factor" if marks is None else None, entities=entities, currency=currency
        )


def cross_section(
    name: str,
    inputs: Iterable[Any],
    outputs: Iterable[str],
    *,
    universe: str | None = None,
    window_days: float = 1.0,
    lag_days: float = 0.0,
    version: int | None = None,
) -> Callable[[Callable], Model]:
    """Decorator: ``f(t, entities, x)`` as a cross-sectional model, called once per time.

    ``t`` is the time in nanoseconds, ``entities`` the universe's names, ``x`` one NumPy array per
    input. ``f`` returns one array per output (a single array for one output) or a dict by output
    name. Inputs are ``"series.column"`` (read at exactly ``t``) or ``fi.asof/market/exact(...)``,
    optionally ``.via("relationship.column")``.
    """

    def wrap(f: Callable) -> Model:
        return Model.python(
            name, f, list(inputs), list(outputs), universe=universe, window_days=window_days, lag_days=lag_days, version=version
        )

    return wrap


def decision(f: Callable | None = None, *, name: str | None = None, version: int | None = None):
    """Decorator: ``f(ctx)`` as a decision, called once per scheduled time with a
    ``DecisionContext``. It returns ``None`` (keep the book), a mapping of contract to weight,
    ``fi.weights(...)`` or ``fi.quantities(...)``."""

    def wrap(g: Callable) -> Decision:
        return Decision.python(g, name=name, version=version)

    return wrap(f) if f is not None else wrap
