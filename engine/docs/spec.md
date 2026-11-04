# The engine: axioms, objects, invariants and interfaces

Normative: the engine satisfies everything on this page, and each claim names the tests that hold
it. What is planned and not built is in its own section at the end, never stated as built.

The core makes seven assumptions and has four primitive objects. Everything else is a definition
or a model built on them: instruments, deals, fair-value, Merton and allocation models, signals,
analytics, policies, execution and strategies.

## Axioms

1. **Observation.** Every input is an observation: a value about an **entity**, with an **event
   time** (when it is true) and a **knowledge time** (when it was known). When the client gives no
   knowledge time, a declared lag policy derives it. Relationships between entities (issuer,
   membership, collateral, underlying) are observations too. What models and decisions see of
   revised values (as known, first release, legacy latest) and which vintage defines outcomes (as
   booked, restated) are two separate, declared policies.
2. **Contract.** Every instrument is a contract: a deterministic state machine for **one unit**. It
   steps over economic time, driven by the observations known at a knowledge time. Each step emits
   the unit's flows and its valuation, or a state saying why there is no valuation (stale, missing,
   defaulted, matured, not yet issued). A unit's state is shared by every holder.
3. **Role.** Every flow and every accrual has a role (income, principal, performance, funding, fee,
   protection, recovery, exchange). Each role maps to a P&L term.
4. **A book is a contract.** A portfolio is a set of holdings (contract, quantity), and is itself a
   contract. A book's liabilities are contracts whose economics come from the book's flows through
   an allocation rule: pro rata for funds, sleeves and composites; a priority of payments for a
   deal's tranches.
5. **Transfer.** Balances change only through balanced transfers, each with its cause in the
   journal: a holding's quantity through a trade, priced by an execution model, or through a
   declared transition of the holding; cash in each currency through flows, trades, settlement,
   interest, financing, margin, distributions and capital.
6. **Model.** Every derived value is the output of a declared model over observations: a fair
   value, a default probability, a signal, a target weight, a unit record. An output's knowledge
   time is never earlier than that of the inputs it used. A model declares its role: signal,
   real-world (P), pricing (Q) or stress.
7. **One producer.** Every state has exactly one producer in a run: an observation, a model or a
   book. An observation may stand in for a derived state, to replay history, only if nothing
   upstream of the state is intervened; an intervention (a stress, a counterfactual decision, a
   simulated world) propagates to everything downstream. A run's mode follows from what it
   substitutes and intervenes.

## The objects

| Object | What it is | Axioms |
| --- | --- | --- |
| **Series** | Typed, columnar values keyed by entity and event time, each with a knowledge time, a unit and its provenance | 1 |
| **Entity** | Anything a series describes, interned to a dense index. Relationships are series of references | 1 |
| **Model** | A declared transform from series to series, of a shape: cross-section, time series or batch | 6 |
| **Graph** | Models wired over series, evaluated in dependency order, in parallel within a stage, cached by content | 6 |

And three built-in kinds of model:

| Kind | What it is | Axioms |
| --- | --- | --- |
| **Contract** | A per-unit state machine whose outputs are unit records | 2, 3 |
| **Portfolio** | Holdings in contracts; itself a contract (a composite) | 4, 5 |
| **Ledger** | A book: holdings × unit records + transfers → P&L terms and NAV, with its policies, execution, financing and distributor (a deal's waterfall) | 3, 4, 5 |

Nothing above this line names a bond, a field such as OAS, a calendar, a frequency, a rating scale
or an optimiser.

## What follows

1. **The identity.** Per unit and interval: income + price + principal + other − financing = ΔV +
   ΣF. Per holding: the quantity × that, plus trade effects. Per book: the sum, which equals ΔNAV.
   The ledger checks it every step and fails the run on a breach.
2. **Two clocks.** The ledger is indexed by economic time. A view at knowledge time K uses only
   observations known by K. The one exception is a declared policy that is not point in time
   (`legacy_latest`), and it is flagged in the manifest.
3. **Measures.** Any quantity per unit is linear in holdings, so exposures, sizing, constraints and
   turnover work in any unit.
4. **Rolls and hedges** are policies of a composite. A composite is held whole or expanded into
   trades.
5. **Obligations are paid from what can be spent when they fall due.** Receipts due settle first.
   A payable or a margin call then takes what cash, and any declared borrowing, allows. The rest
   stays owed, and is overdue: cash never goes below its floor to pay it. The declared responses
   then run, and whatever they cannot meet is an economic failure (axiom 5).

## Debt from its terms

A debt instrument is one contract (`fi_std::DebtContract`) over terms (`fi_terms::Debt`), never a
type per product.

- **Conventions** are definitions: day counts (ACT/360, ACT/365F, ACT/365.25, ACT/ACT ISDA, ACT/ACT
  ICMA with its reference period and stubs, 30/360 bond basis, 30/360 US, 30E/360, 30E/360 ISDA);
  business calendars from a declared weekend and holidays, given as dates or as rules (fixed with
  observance, n-th weekday, Easter offsets), joined; business-day conventions. Nothing names a
  country.
- **Schedules** roll from maturity or from first and last regular dates, or take explicit dates,
  with short or long stubs, the end-of-month rule, adjusted payment dates (accrual dates too if
  declared), payment lags and ex-coupon dates in business days before payment.
- **Coupons**: fixed; stepped; floating (`gearing × index + spread`, capped and floored, fixed in
  advance with a lag, or compounded or averaged in arrears with lookback and lockout); inflation
  (a real rate on indexed principal, with lag, interpolation and a deflation floor); zero.
- **Principal**: bullet, an amortisation or sinking schedule at a price, and a soft-bullet
  extension. **Rights**: calls and puts over windows at a price, with notice and a make-whole
  spread. **Rounding**: each contractual amount per unit, to the decimal places the terms say.
- **Entitlement.** A coupon is earned by whoever holds the unit when it goes ex (at its period's end
  without an ex period) and paid on its payment date; between the two it is a receivable of the
  holder at ex, whoever holds the unit when it pays (a deferred flow). Accrued interest comes from
  the same schedule, negative once ex, so it cannot disagree with the coupon.
- **Events** observed per instrument move it: a call or put at its price, checked against its
  rights; a default (accrual stops, accrued interest is written off, it trades flat); a recovery,
  paid at once or determined now and paid later; a write-down or write-up; a reported factor; an
  extension. A bond before its issue date is pending: no value, nothing can trade it.
- **Valuation** is the clean price per 100 of current face plus accrued interest. The clean price
  is observed, or comes from an observed yield through the terms, or from a curve of observed zero
  rates plus a spread; a floater's coupons not yet fixed are valued at the curve's forwards, while
  its cash comes from observed fixings alone.
- **Analytics** (`fi_terms::analytics`): prices from curves and yields (street, annual,
  continuous), yield and Z-spread solvers, durations and convexity, and sensitivities by
  central difference to a declared bump (parallel, key rate, spread), each returned with the
  definition of what it measured.
- **Attribution** (`Explained`, `ws.explain`, `fi.attribution`): a step of a debt valued off a curve
  is repriced off the old curve unchanged in tenor and the old spread (carry: the accrual, the
  flows and the roll down the curve), off the new curve at the old spread (the curve), and as
  marked (the spread); they add up to its total P&L. A book's attribution weights each contract's
  by the units held over the interval, and names what no holding earned (cash interest,
  financing, costs, currency). A step with a credit event or a repayment is left unexplained.
- **Supplied histories.** A bond whose marks and flows are supplied (`SuppliedBond`) checks that
  its accrued interest falls back when a coupon pays, so a coupon is never counted twice.

Tests: `fi-terms`'s own, `fi-std`'s debt contract tests, `python/tests/test_debt.py`.

## Swaps and CDS

- **An interest rate swap** (`fi_std::SwapContract`, `ws.swap`) is a received leg less a paid
  leg, each a debt's terms on one unit of notional, their principal exchanges cancelling, both
  stepped as debt and valued off one curve; its value, flows and P&L are the legs' differences.
- **A CDS** (`fi_std::CdsContract`, `ws.cds`), single-name or index: one unit of notional of
  protection bought. The premium is the coupon on the notional outstanding, on a declared
  schedule; each reference entity's credit event pays one less its recovery on its weight, less
  the premium its part accrued, and reduces the notional by its weight. Valued from a price quoted
  per 100 from the seller's side.
- Both are unfunded: a negative quantity needs no borrowed security.

Tests: `python/tests/test_debt.py` (a swap is its legs' difference), `test_acceptance_debt.py`
(the brief's R5).

## Exchange-traded futures

A future's terms are a Nautilus `FuturesContract` (`fi_exec::ExchangeFuture`; `ws.future` builds
one through Nautilus's own builder and checks). One unit is one contract, marked to market at each
step: variation margin, the multiplier times the change in the settlement price on the tick, as a
performance flow, so a unit's value is zero. Before activation it is pending; at expiration it
settles a last time and expires. A roll is a policy (`fi_exec::Roll`): a number of contracts in
the first contract of a chain more than a given number of days from expiry, moved to the next in
one decision, so the contract change, the margin and the exposure reconcile through the same
identity.

Tests: `python/tests/test_futures.py` (the brief's R4).

## The optimiser

`fi_std::Optimise` (`fi.Decision.optimise`) is a decision: at each, the weights that maximise
expected return less a risk penalty and a turnover penalty, under weight bounds, the budget,
exposure bands in any per-contract measure and group caps, solved by ADMM (as OSQP) with the few
dense constraint rows taken by the Woodbury identity. The solver's status and the constraints
that bind are journalled with the decision; an infeasible mandate keeps the book (and says so) or
stops the run, as declared: nothing is relaxed. Tests: its own against the water-filling closed
form, and `python/tests/test_optimise.py` (every constraint holds in the book traded to).

## The ledger's step

Each step, at time t:

1. **Contract events** (`ledger/flows.rs`): the held units' flows over the interval by role,
   translated at the new FX rates; deferred flows booked as receivables; flows on units traded but
   not delivered owed on to their owner. Then interest on cash and financing, and borrow fees.
2. **Holding transitions** (`ledger/transitions.rs`) declared with the records: every unit of a
   contract becomes the declared units of others and cash, at both sides' values at t, the
   difference booked once to the declared term. Undelivered units follow the new claim; working
   orders in the old contract are cancelled; the old contract keeps its records.
3. **The declared phases** (`StepOrder`, `ledger/order.rs`), by default: weight resets, capital
   flows, decisions and orders, execution, settlement, recalls, distribution, agreements, the cash
   guard, constraints. Every phase runs once, execution after decisions; decisions see the NAV
   before the step's capital flows. The order is part of the book's settings, so of its manifest;
   a book declares another to test whether an ordering assumption is material.
4. **The mark:** NAV, checked against the identity.

Tests: `fi-kernel`'s, `python/tests/test_books.py` (the declared order),
`python/tests/test_transitions.py` (the brief's P3).

## Invariants

| # | Invariant | Held by |
| --- | --- | --- |
| I1 | **Identity.** The terms add up exactly: cash to the smallest unit of its decimals, valuations to 1e-12 relative | `invariants.rs` (random contracts, holdings and trades), and every run's own check |
| I2 | **Point in time.** Changing any observation known after K changes no view at K, and no decision taken at or before K | `invariants.rs` (perturb only the future) |
| I3 | **Composites.** Holding a composite whole, or expanded into trades, gives the same P&L when roll and hedge costs are equal | `invariants.rs` |
| I5 | **Determinism.** Results do not depend on the number of threads or on the order of input rows | `invariants.rs`; the same results byte for byte in two processes (`python/tests/test_fi.py` at the repo's root) |
| I7 | **Conservation.** A structure's allocations plus its residual equal its collections, every period | `structured.rs` (F02, F14), and every collateral book's identity |
| I8 | **Nothing silent.** Every value that was not observed carries the policy that produced it, and the run's report counts it | `invariants.rs`, the records' diagnostics |
| I9 | **Models are point in time.** A model's output at (t, K) depends only on inputs with event time ≤ t that were known by K | `invariants.rs`, `generality.rs` |
| I10 | **Reproducible.** The same input versions, parameters and policies give bit-identical outputs, so cached outputs are always valid | `manifest.rs` (hashes recomputed and compared) |

## Interfaces

Each family is a Rust trait with built-ins, and a Python bridge where Python users extend it.

| Interface | Role |
| --- | --- |
| `Contract`, `Stepper` | A definition binds to its series once per pass, then steps one unit: `init(t0)`, `step(from, to) → UnitStep` (state, value, flows, terms, face, deferred cash). `supports()` lists what it models, checked against a run's requirements |
| `CrossSectionModel`, `TimeSeriesModel`, `BatchModel` | Models by shape, over causal views |
| `DecisionModel` | A policy's decision: weights, quantities, orders or targets in a measure, from what it may see |
| `FillModel`, `Execution` | Orders into fills, alone or worked together over time |
| `Distributor` | An allocation rule over a book's collections (a waterfall) |
| `Measure` | A quantity per unit, linear in holdings |
| `Progress` | Where long loops report (the watcher's ring under a supervisor) |

## Numbers

- **Cash** is a fixed-point decimal, an `i128` with 16 decimal places, as Nautilus's high-precision
  `Money`. A float becomes cash as the decimal it states (its value to 15 significant digits, at
  most 16 places, rounded half to even in integers), and sums are exact.
- **Flows are decimal products.** A book's flow is its quantity times the amount per unit,
  multiplied exactly in decimal (`Cash::product`), for contract flows, deferred receivables, owed
  coupons, trades and transitions.
- **Contractual amounts round as the contract says.** A debt's terms round each amount per unit to
  their decimal places. A deal's rounding term (`Rounding`) is a unit and a mode (down, up, half up,
  half even): every amount due and paid is a whole number of units, a pro rata split gives each odd
  unit to the largest remainder, a cure is the smallest whole amount that cures. A currency's minor
  unit comes from Nautilus's currency registry (`fi_exec::rounding`).
- **Valuations, FX translation and measures** are `f64`. **Quantities** are `f64`.
- **Reductions** run in a fixed order, so threads cannot change the result (I5).

## Performance budgets

On a synthetic universe of 9,000 contracts × 2,520 days, on 16 cores (`make bench-full`; `make
bench` runs them at a quarter scale, scaled):

| Work | Budget |
| --- | --- |
| Unit records for simple valuations (marks plus accrued) | 2 s |
| One research run on cached records: monthly rebalance, 300 holdings, no optimiser | 50 ms |
| Sweeps | Linear in the number of variants, in parallel across cores |
| Frames in and out of Python | Zero copy (Arrow's C data interface) |

Unit records are computed once per contract and policy set, and shared by every variant.

## Not built

- **An event-driven driver on Nautilus** (its engine running the same strategies), so neither the
  parity of that fidelity with research (formerly I4) nor a mirror of Nautilus's balances (I6) is
  claimed. Live trading is not in scope.
- **Model shapes** beyond cross-section, time series and batch (elementwise, panel, event).
- **Lots by specific identification.**
- Whatever the review's resolution section still lists as open.
