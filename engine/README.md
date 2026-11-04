# fi-engine

A backtesting and simulation engine for fixed income, built into gongoozler. The core is a few
general Rust objects that assume almost nothing about debt; instruments, deals, models and
strategies are definitions built on them. The engine is configured from Python only (there are no
configuration files) and fed with any Arrow frame (Polars, pandas, pyarrow, DuckDB).

- **Debt from its terms** (`fi-terms`): day counts, business calendars from declared holidays,
  schedules with stubs, payment lags and ex-coupon periods, fixed, stepped, floating and
  inflation-linked coupons, amortisation, calls, puts and extensions; prices from curves and
  yields, yield and Z-spread solvers, durations and sensitivities that state their bump.
- **A ledger that books exactly:** P&L by role checked against NAV every step, cash in exact
  decimals, trade and settlement dates, receivables, a coupon gone ex paid to its seller, holding
  transitions (exchanges, conversions, delayed recoveries), financing agreements and margin, and a
  step order that is declared, recorded and variable.
- **Point in time by construction:** every input is a bitemporal observation, and decisions see
  only what was known.
- **Deals:** CLO and ABS waterfalls, tests, triggers, rights and manager counterfactuals.
- **Nautilus and Polars where they are the established implementation:** futures are Nautilus
  `FuturesContract`s (multiplier, tick, listing and expiry as Nautilus checks them) and roll as a
  policy; Nautilus's L2 order book fills orders, its currency registry gives minor units and its
  analysis crate the statistics;
  Polars carries frames in and out without copying.
- **Watched:** under `gongoozler run`, the engine's Rust loops report their progress into the
  watcher's ring, and `gongoozler.fi` keeps a run's results (spec/results.md at the repo's root).

The normative spec is [docs/spec.md](docs/spec.md). The review against the platform brief, with
how each finding was resolved, is [../review/2026-11-03-engine.md](../review/2026-11-03-engine.md).

## The core

**Seven axioms.**
1. Observations are bitemporal: each value has an event time and the time it became known.
   Relationships, such as a bond's issuer, are observations too. What a decision sees of revised
   values, and which vintage defines what happened, are separate choices.
2. A contract is a deterministic state machine per unit, and its state is shared by every holder.
3. Every flow has a role: income, principal, performance, fee, protection, recovery, exchange or
   funding.
4. **A book is a contract.** A portfolio, a sleeve or a continuous future is a composite. A
   deal's waterfall is the allocation rule over its collateral book, and its tranches are
   contracts on that allocation.
5. **Balances change only through balanced transfers:**
   - cash in each currency;
   - receivables and payables until settlement;
   - financing agreements, posted collateral and capital, each with its cause in the journal.
6. Every derived value is a point-in-time model output.
7. **Every state has one producer in a run:** an observation, a model or a book. An observation
   may stand in for a derived state only if nothing upstream of it is intervened. A run's mode
   (replay, policy backtest, scenario, counterfactual, simulation) follows from that, rather than
   being declared.

**Four primitives.**
- **Series:** typed columns keyed by entity, event time and knowledge time, with their provenance.
- **Entity:** anything a series describes, interned to a dense id.
- **Model:** a declared, point-in-time transform from series to series, with a role:
  - a signal;
  - a real-world (P) model;
  - a pricing (Q) model;
  - a stress.
- **Graph:** models wired over series, evaluated in parallel and cached by content.

**Built-in kinds of model.**
- **Contract:** its output is unit records, the value, cash and P&L terms of one unit at each
  time.
- **Portfolio:** a set of holdings that is itself a contract (a composite).
- **Ledger:** a book stepped through time. It holds:
  - policies with their own clocks;
  - an execution process, settlement, the cash and short guards, financing agreements, capital
    flows and constraints;
  - a distributor such as a deal's waterfall.
- **Batch model:** a book or a deal that steps its own state and writes series.

**The accounting identity.** Every unit's terms add up to its change in value plus its cash:
- income = ΔA_inc + F_income
- price = Δclean + R + F_performance
- principal = F_principal − R
- other
- − financing

The ledger keeps cash exact (i128 fixed point). Every step it checks NAV(k) − NAV(k−1) = income +
price + principal + other − financing + currency − costs + capital.

**What the core never names:** a bond, a field, a calendar, a frequency, a rating scale, a
priority of payments or an optimiser. All of those are definitions. Errors stop the work and say
where; nothing is silently skipped or defaulted.

## Crates

| Crate | Holds | Uses |
| --- | --- | --- |
| `fi-terms` | Debt from its terms: dates, day counts, business calendars, schedules, coupons, debt terms and their cash flows, curves, and analytics (prices, yields, Z-spreads, durations, sensitivities) | nothing |
| `fi-kernel` | Series, entities, models and the graph (substitution, interventions, modes); contracts, unit records, composites, measures, holding transitions; the ledger (its phases in their declared order, currencies, settlement, guards, orders, policies, constraints, financing, distributors, journal, checkpoints); exact cash; progress | `rayon` only |
| `fi-std` | Definitions on the kernel:<br>• contracts: debt from its terms (`DebtContract`), total return, bonds with supplied marks and flows (reconciled), cash, daily-settled futures<br>• deals: collateral books with their waterfalls, tests, triggers, reserves, deficiency ledgers and rights<br>• world generators: Gaussian paths, correlated defaults<br>• models: z-score, Nelson–Siegel, EWMA volatility, Merton<br>• decisions: top N, risk parity, replay, recorded trades, branches, reinvestment<br>• the synthetic universe | `fi-kernel`, `fi-terms`, `rayon`, `libm` |
| `fi-research` | Books, runs and parallel sweeps; run modes; NAV reconciliation; worlds; the run manifest, capability checks and the support matrix | `fi-kernel`, `rayon` |
| `fi-io` | Frames in, checked against their declarations; results out as frames | Polars 0.55 |
| `fi-exec` | Exchange-traded futures whose terms are Nautilus's `FuturesContract` (`ExchangeFuture`) and rolls between them (`Roll`). Fills: Nautilus L2 order books, request for quote, recorded quotes, bid lists (auctions, whole lines or none), primary allocation at the reoffer. Execution processes: RFQ over time, portfolio trades and shared capacity | `nautilus-model` 0.67 |
| `fi-metrics` | Sharpe, Sortino, drawdown, CAGR, tracking error, capture ratios | `nautilus-analysis` 0.67 |
| `fi-ring` | The watcher's ring (gongoozler's spec/ring.md), written from Rust | `memmap2`, `crc32fast` |
| `fi-python` | The `fi_engine` Python module | pyo3 0.30, pyo3-arrow 0.20 |
| `fi-bench` | Speed tests against the spec's budgets | |

`fi-kernel` sits at the bottom, and nothing depends upward.

## From Python

```sh
make python                    # creates .venv (uv) and builds the extension, release
.venv/bin/python examples/quickstart.py
gongoozler run examples/quickstart.py    # the same file, watched (from the repo's root: engine/examples/…)
```

**Debt from its terms.** One instrument, or a universe from a terms table:

```python
import fi_engine as fi

ust = fi.Bond("2024-02-15", "2034-02-15", 0.04, day_count="ACT/ACT ICMA", frequency=2)
ust.cashflows()                                    # Polars: kind, date, pay, ex, accrual, fraction, rate, amount
ust.price("2025-03-03", yield_=0.045)              # {"clean", "accrued", "dirty"} per 100
ust.risk("2025-03-03", fi.Curve("2025-03-03", [1, 5, 10], [0.04, 0.042, 0.045]))   # with its definition

frn = fi.Bond("2024-01-02", "2029-01-02", fi.Coupon.floating("SOFR", spread=0.01, method="compounded", lookback=2),
              day_count="ACT/360", frequency=4, calendar=fi.BusinessCalendar("NY", holidays))
bonds = fi.bonds(terms, calendars={"NY": ny}, rights=calls)     # name -> Bond, from frames

contracts = ws.debt(bonds, fi.valuation.curve("curves", "UST", {"z1": 1, "z5": 5, "z10": 10}, spread="marks.spread"),
                    fixings="fixings.rate", events="events")    # calls, defaults, recoveries, factors…
records = ws.records(contracts, cal, transitions=[fi.transition("2025-04-01", "OLD", {"NEW": 0.5}, cash=0.02)])
```

**Data, models and strategies:**

```python
ws = fi.Workspace()                        # decisions see each value as it was known
ws.series("marks", marks, entity="isin", kind="bond", time="date", known=fi.lag(hours=18),
          values={"clean": "price", "accrued": "price", "oas": "bp"}, grid=True)
ws.series("flows", flows, entity="isin", kind="bond", time="date", values={"interest": "number"})
ws.series("sector", sectors, entity="isin", kind="bond", values={"sector": "category"})

ws.evaluate([fi.Model.zscore("cheap", "marks.oas", group="sector.sector")], ws.calendar("marks"))
records = ws.records(ws.supplied_bonds("marks", flows="flows"), ws.calendar("marks"))

@fi.decision
def cheapest(ctx):                         # called once per rebalance, point in time
    z = ctx.cross_section("cheap", "z")
    best = np.argsort(-np.nan_to_num(z, nan=-np.inf), kind="stable")[:20]
    return {ctx.contracts[i]: 1 / 20 for i in best}

runs = ws.sweep([fi.Strategy(fi.Decision.top_n("cheap.z", 20), fill=fi.Fill.research(5)),
                 fi.Strategy(cheapest, fill=fi.Fill.research(5))], records)
runs[0].ledger()                           # Polars: nav, cash, income, price, principal, ..., currency, capital
runs[0].report()                           # Nautilus's statistics
```

**Books.** `fi.Book` takes the following:
- **policies:** each a `fi.Policy` with a decision, a schedule, a scope, a priority and a lag;
- **an execution:** a `fi.Fill`, or a `fi.Execution` process (RFQ over time, portfolio trades,
  shared capacity);
- **the book's terms:**
  - a base currency and FX;
  - settlement lags;
  - a cash guard (`forbid` is the default; `borrow` or `unlimited` must be named);
  - short borrows;
  - capital flows (`fi.flows.series` or `fi.flows.model`);
  - `fi.Constraint` rules;
  - opening positions;
  - financing (`fi.Agreement.secured` for repos and warehouses, `fi.Agreement.csa`);
  - the responses to a cash need, in order.

A run reports its mode, journal, orders, holdings, financing and any economic failure.

**Deals.** `ws.deal(...)` builds a securitisation as a model:
- a collateral book with its manager's policies;
- tranches (`fi.Tranche`), fees, coverage tests (`fi.Test.oc` and `.ic`), haircuts, triggers,
  rights and exercises;
- two priorities of payments as string steps, such as `"interest:A"` or
  `"sequential:A,B when amortising"`.

Evaluating it writes the tranches' flows and marks, and `ws.tranches("clo")` makes them contracts
any book can hold.

**Worlds.** `ws.world()` is a copy that shares every series. In it, `ws.gaussian_paths` and
`ws.defaults` write a simulated world with keyed draws: each world is reproducible, and two
policies compared on it share its draws.

**The run's records.** `ws.evaluate(..., substitute={state: observation})` replays history, and
is refused downstream of an intervention. `ws.reconcile(run, "admin.nav")` compares a run's NAV
with an official one, and differences stay visible. `ws.manifest(run, book, records)` pins
everything the output depends on; `.diff` attributes any change. `ws.records(...,
requires=..., approximate=...)` refuses definitions that do not model what a run needs, unless the
gap is declared.

**Feeding data and models.**
- **Frames.** A frame is declared as a series, and the declaration is the contract it is checked
  against when it arrives: types, units, nulls in keys, NaN, infinities and duplicates. Only the
  declared columns are read. Text relationships (`fi.ref("issuer")`) and categories are interned,
  and they come back as names.
- **Knowledge times** come from a column, a fixed lag (`fi.lag(hours=18)`) or a lag in rows
  (`fi.lag(rows=1)`). A `scenario=` series is labelled as one, and so is everything derived from
  it.
- **Python models** are functions `f(t, entities, x)` called once per time with NumPy arrays
  (`fi.cross_section`). They declare a role (`.role("pricing")`), an availability date, or
  versions (`Model.vintaged`).
- **Python decisions** are functions `f(ctx)`:
  - they read point in time through `ctx.cross_section`, `ctx.value` and `ctx.returns`;
  - they see `ctx.holdings`, `ctx.orders`, `ctx.cash` and, in a deal, `ctx.state("reinvesting")`;
  - they return weights, quantities or orders.
- **Errors.** A Python exception comes back to the caller unchanged, and the work stops.
- **Legacy parity.** How other frameworks count is a setting, named: `holding="reset_weights"`,
  `Workspace(information="legacy_latest")`, the data's own calendar, and lags in rows.

**Examples** (`examples/`, run files, each run by the tests):
- `quickstart.py`;
- four configurations of the platform brief, in Python:
  - `e1_historical_multiasset.py`;
  - `e2_clo_counterfactual.py`;
  - `e3_forward_monte_carlo.py`;
  - `e4_holdings_reconciliation.py`.

The repo's `examples/fi_bonds.py` and `fi_in_a_box.py` run the engine under the watcher, with
bonds from their terms.

## From Rust

```rust
let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
fi_io::register(&mut ws, &marks_df, &SeriesDecl::new("marks").entity("isin", "bond").time("date")
    .value("clean", Unit::Price).value("accrued", Unit::Price).value("signal", Unit::Number).grid())?;
let contracts: Vec<Arc<dyn Contract>> = /* fi_std::SuppliedBond per bond */;
let prepared = fi_research::prepare(&mut ws, &graph, &contracts, &calendar, MissingPolicy::Fail)?;
let out = fi_research::run_book(&ws, &prepared.records, &book)?;
let manifest = fi_research::Manifest::of(&ws, &prepared.records, &reads, &book, &out, &[]);
```

A custom definition implements a trait, and the kernel does not change:
- a contract implements `Contract` and `Stepper`;
- a model implements `CrossSectionModel`, `TimeSeriesModel` or `BatchModel`;
- a decision implements `DecisionModel`;
- an execution process implements `Execution`;
- an allocation rule implements `Distributor`.

## Tests

`make check` runs rustfmt, clippy and every test; `make help` lists the targets.

| Where | What it checks |
| --- | --- |
| `crates/fi-terms` | Day counts against ISDA's definitions, calendars and adjustment, schedules with stubs and ex dates, coupons in advance and in arrears, cash flows, prices against closed forms, solvers, sensitivities |
| `crates/fi-kernel` | Exact cash and decimal products, the step order, transitions, the identity per unit |
| `crates/fi-std` | The debt contract against hand calculations: coupons, ex-coupon entitlement, calls, defaults and recoveries, floaters, yields |
| `crates/fi-research/tests/invariants.rs` | The spec's invariants as property tests and scenarios |
| `…/generality.rs` | Very different problems through the same objects: a curve fit, Merton per issuer, risk parity over sleeves, replays, sizing in any measure, knowledge policies |
| `…/books.rs` | The identity across currencies (property tests), the cash guard, settlement and coupon entitlement (R1), borrowing, shorts, capital flows, the horizon, orders over time (R6, P10), policies, constraints, shared capacity (P11), checkpoints (P12) |
| `…/structured.rs` | Deals: F01–F06, F14, F18, R9, R12, tranches held by other books, whole-cent waterfalls; one producer per state (F03, R10, P6), stresses, run modes, reconciliation (P2) |
| `…/financing.rs` | Haircuts as funding events (R8), borrowing bases (F20, R14), maturities and takeouts (F21), redemptions funded or failed (R7), annexes per netting set (F22, P9), margin times, capacity rounding |
| `…/worlds.rs`, `…/manifest.rs` | Waterfalls inside each world (F25), shared borrowers (F23), reproducible worlds; pinned reruns, model upgrades, retries that duplicate nothing (P12) |
| `python/tests` | The same from Python, and debt from its terms through the ledger (`test_debt.py`), transitions and the brief's P3 (`test_transitions.py`), the declared step order, the brief's acceptance cases with its own numbers, the examples, and the speed budgets |

The case names (F01–F25, R1–R15, P1–P12) are the platform brief's acceptance cases.

## Speed

`make bench` checks the spec's budgets at a quarter of full scale; `make bench-full` at the spec's
scale, 9,000 bonds × 2,520 days (about 3.5 GB of memory; CI runs it). Rust, at a quarter scale
(2,250 bonds × 2,520 days), 16 threads, 3 Nov:

| Test | Seconds | Budget | Note |
| --- | ---: | ---: | --- |
| Unit records: marks, accrued and flows | 0.114 | 0.5 | 50 M records/s |
| Z-score by sector, through the graph | 0.056 | 0.5 | 1.9× a hand-written loop; same outputs |
| EWMA volatility, through the graph | 0.074 | 0.5 | 2.0× a hand-written loop |
| One research run: top 300, monthly, 116 rebalances | 0.042 | 0.05 | 59,322 trades; identity error 8e-15 |
| Sweep: 64 variants in parallel | 0.324 | 0.591 | 7.3× serial; same accounts |

The research run's budget is not scaled: its cost depends on holdings and days, not the universe.

## Status

What is not built, and stands as a finding in the review until it is: see its resolution section.

## Layout

```
crates/        the Rust workspace (see Crates)
python/        the fi_engine package (python/fi_engine) and its tests (python/tests)
examples/      run files, run by the tests
docs/          spec.md
```

The toolchain is pinned in `rust-toolchain.toml` and formatting in `rustfmt.toml`. `make clippy`
and `make fmt` check them.
