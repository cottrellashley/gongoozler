"""Deals: books whose liabilities are contracts (docs/design/platform.md, change 3).

A deal is a collateral book and its priority of payments. ``Workspace.deal(...)`` builds it from
these terms as a model; evaluating it writes the tranches' flows and marks as series, and
``Workspace.tranches(deal)`` makes them contracts that any book can hold.

    clo = ws.deal(
        "clo", calendar,
        collateral=loans, opening={"L1": 60.0, "L2": 40.0},
        tranches=[fi.Tranche("A", 80.0, coupon=fi.floating("sofr.rate", spread_bp=150)),
                  fi.Tranche.equity("E", 20.0)],
        tests=[fi.Test.oc("A_OC", ["A"], 1.15, cure=["A"])],
        interest=["interest:A", "test:A_OC", "residual:E"],
        principal=["sequential:A when amortising"],
        payments=quarter_ends, reinvest_until="2027-01-15",
    )
    ws.evaluate([clo], calendar)
    ws.frame("clo.tests")

The engine imposes no order: the two lists of steps are the deal's own. A step is
``action[:names] [when condition]``:

* ``fee:NAME``, the fee due and any carried;
* ``interest:A,B``, interest due and deferred, pro rata. ``interest+reserve:A`` draws a shortfall
  from the reserve;
* ``sequential:A,B`` and ``pro_rata:A,B``, principal. ``schedule:PAC`` pays down only to the
  scheduled balance;
* ``test:NAME``, a coverage test, and its cure if it fails;
* ``reserve``, which tops the reserve up to its target;
* ``deficiency:A,B``, which cures deficiency ledgers by moving interest into principal;
* ``to_principal`` and ``to_interest``, which move what is left;
* ``residual:E``, which pays what is left.

Conditions are ``reinvesting``, ``amortising``, a trigger's name, or ``not`` a trigger's name.

An indenture that pays whole cents says so: ``rounding=("USD", "half_up")``. The unit is a number
or a currency code, whose minor unit comes from Nautilus's currency registry. The mode is
``"down"``, ``"up"``, ``"half_up"`` or ``"half_even"``. Every amount due and paid is then whole
cents, a pro rata split gives each odd cent to the largest remainder, and a cure is the smallest
whole-cent paydown that cures.
"""

from __future__ import annotations

from collections.abc import Iterable, Mapping
from dataclasses import dataclass
from typing import Any

__all__ = [
    "Exercise",
    "Fee",
    "Floating",
    "Haircut",
    "Right",
    "Test",
    "Tranche",
    "Trigger",
    "deficiency_ledger",
    "floating",
    "write_down",
]


@dataclass(frozen=True)
class Floating:
    """A floating coupon: a market-wide index (``"series.column"``, read as known at the start of
    each accrual period) plus a spread, with an optional floor on the total."""

    index: str
    spread_bp: float = 0.0
    floor: float | None = None


def floating(index: str, *, spread_bp: float = 0.0, floor: float | None = None) -> Floating:
    """A floating coupon on ``index`` plus ``spread_bp``."""
    return Floating(index, spread_bp, floor)


@dataclass(frozen=True)
class Tranche:
    """One of a deal's claims. List tranches most senior first.

    * ``coupon``: ``None``, a fixed rate (a fraction), or ``fi.floating(...)``.
    * ``shortfall``: what happens to interest not paid. ``"default"`` (owed, and an event of
      default), ``"defer"``, ``"defer_compound"`` (deferred interest earns the coupon),
      ``"capitalise"`` (PIK) or ``"extinguish"``.
    * ``claim``: ``"debt"``, ``"equity"`` (what is left) or ``"interest_only"`` (interest on a
      ``notional`` following ``"pool"`` or a tranche's balance, times ``ratio``; its fall is
      written down, never paid).
    * ``balance``: the balance at the first time, for a seasoned deal.
    * ``basis``: days in a year (Act/basis).
    * ``issued=False``: a replacement note, issued by an exercise.
    * ``schedule``: scheduled balances, ``{date: balance}`` (a planned amortization class).
    """

    name: str
    face: float
    coupon: float | Floating | None = None
    shortfall: str = "default"
    claim: str = "debt"
    notional: str = "pool"
    ratio: float = 1.0
    balance: float | None = None
    basis: float = 360.0
    issued: bool = True
    schedule: Mapping[Any, float] | Iterable[tuple[Any, float]] | None = None

    @staticmethod
    def equity(name: str, face: float) -> Tranche:
        """The residual claim."""
        return Tranche(name, face, claim="equity", shortfall="extinguish")

    @staticmethod
    def interest_only(name: str, notional: float, coupon: float | Floating, *, of: str = "pool", ratio: float = 1.0) -> Tranche:
        """Interest on a notional that follows ``of`` (``"pool"`` or a tranche), with no claim to
        principal."""
        return Tranche(name, notional, coupon=coupon, claim="interest_only", notional=of, ratio=ratio)

    @staticmethod
    def replacement(name: str, face: float, coupon: float | Floating, **terms: Any) -> Tranche:
        """A note that a refinancing or reset issues."""
        return Tranche(name, face, coupon=coupon, issued=False, **terms)


@dataclass(frozen=True)
class Fee:
    """A fee: ``rate_bp`` a year on the pool's par at the start of the period (Act/360), plus
    ``fixed`` per payment date. Unpaid amounts are carried if ``carry``."""

    name: str
    rate_bp: float = 0.0
    fixed: float = 0.0
    carry: bool = True


@dataclass(frozen=True)
class Haircut:
    """A rule of the adjusted collateral. The holdings flagged by ``when`` (``"series.column"``,
    non-zero where it applies, as known at the test) count at the lesser of ``carry``, per unit
    of par. Each carry is ``"market"``, a ``"series.column"`` fraction, ``"series.column/100"``
    (a price per 100), or a number. With ``excess_over``, only the part above that fraction of the
    remaining par is haircut, lowest carried first (excess CCC). Rules apply in order: a holding
    one rule takes is not seen by the next."""

    name: str
    when: str
    carry: tuple[Any, ...] = ("market",)
    excess_over: float | None = None


@dataclass(frozen=True)
class Test:
    """A coverage test over ``classes`` (this class and every senior one).

    It passes at or above ``threshold``. If it fails, ``cure`` applies: ``None`` (report only),
    tranche names to pay down in order, or ``"reinvest"`` (move interest into principal).
    """

    name: str
    kind: str
    classes: tuple[str, ...]
    threshold: float
    cure: Any = None

    @staticmethod
    def oc(name: str, classes: Iterable[str], threshold: float, *, cure: Any = None) -> Test:
        """Par coverage: the adjusted collateral and principal cash over the classes' balances."""
        return Test(name, "oc", tuple(classes), threshold, _cure(cure))

    @staticmethod
    def ic(name: str, classes: Iterable[str], threshold: float, *, cure: Any = None) -> Test:
        """Interest coverage: interest collected less fees paid so far, over the classes'
        interest."""
        return Test(name, "ic", tuple(classes), threshold, _cure(cure))


def _cure(cure: Any) -> Any:
    if cure is None or isinstance(cure, str):
        return cure
    return tuple(cure)


@dataclass(frozen=True)
class Trigger:
    """A persistent condition, observed on each payment date before the waterfall.

    * ``metric``: ``"cumulative_loss"``, ``"pool_factor"``, ``"excess_spread"``,
      ``"payment_rate"``, ``"test:NAME"`` or a market-wide ``"series.column"``.
    * It trips when the average over ``lookback`` payment dates is above ``above`` or below
      ``below``. Exactly at the boundary, it does not trip.
    * A ``curable`` trigger clears when the metric recovers.
    * ``ends_reinvestment``: an early amortization event.
    """

    name: str
    metric: str
    above: float | None = None
    below: float | None = None
    lookback: int = 1
    curable: bool = False
    ends_reinvestment: bool = False


@dataclass(frozen=True)
class Right:
    """A right in the deal's terms: ``"call"``, ``"refinance"`` or ``"reset"``. The ``holder``
    class may exercise it with ``majority`` consenting, not before ``non_call``. A passive
    holding confers none."""

    kind: str
    holder: str
    majority: float = 0.5
    non_call: Any = None


@dataclass(frozen=True)
class Exercise:
    """An exercise of a right on a payment date. A refinancing or reset ``redeem``s classes one for
    one with their ``replacements``; ``expenses`` are paid from interest (the exercising class
    contributes the rest); a reset sets ``reinvest_until``."""

    kind: str
    date: Any
    by: str
    share: float = 1.0
    redeem: tuple[str, ...] = ()
    replacements: tuple[str, ...] = ()
    expenses: float = 0.0
    reinvest_until: Any = None


def write_down(order: Iterable[str], *, write_up: bool = False) -> tuple[str, list[str], bool]:
    """Losses write tranches down in ``order``, most junior first; with ``write_up``, recoveries
    write them back up."""
    return ("write_down", list(order), write_up)


def deficiency_ledger(order: Iterable[str]) -> tuple[str, list[str], bool]:
    """Losses are debited to the tranches' deficiency ledgers in ``order``, most junior first, and
    cured where the waterfall lists ``deficiency``."""
    return ("ledger", list(order), False)
