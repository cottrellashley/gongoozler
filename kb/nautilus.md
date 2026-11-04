# What we rely on from NautilusTrader

Pinned: the `nautilus_trader` 2.0.0rc8 wheel (PyPI, released 29 Oct 2026; wheels for CPython
3.13 and 3.14; `requires_python <3.15,>=3.13`). Docs read at develop `8b283ac`
(`~/workspace/_sources/nautilus_trader`, a read-only clone). "Read" means taken from the docs;
"observed" means seen running the wheel, in this repo's tests or spikes.

| Fact | Source | Used by |
| --- | --- | --- |
| The streaming loop: `add_data(batch)`, `run(streaming=True)`, `clear_data()` per batch, then `end()`. `run(streaming=True)` "pauses when the current data is exhausted"; `end()` "flushes timers to the last run boundary, invokes stop handlers, and produces the final result" | Read: `docs/concepts/backtesting/apis-and-runs.md`, "Streaming batches manually" | `gongoozler.nautilus.run` |
| Streaming gives the same result as one run, timers included | Observed: `tests/test_nautilus.py`, and `tools/spikes/stream_equivalence.py` (10 and 1,000 batches) | The design's central bet |
| Ties: "Across streams: The stream registered by the earlier `add_data()` call takes priority. Within a stream: Items retain their order in the input list" | Read: `apis-and-runs.md`, "Equal-timestamp replay ordering". Observed: `test_two_feeds_keep_nautilus_tie_order_and_timers` | `windows` |
| `add_data(data, client_id=None, validate=True, sort=True)`. With `sort=False`, `run` refuses: "Data has been added but not sorted, call `engine.sort_data()` or use `engine.add_data(..., sort=true)` before running" | Observed: the 2.0 stub, and the error | The driver keeps the default |
| Python actors subclass `nautilus_trader.common.DataActor`, a Rust class whose constructor takes at most one argument (`DataActor.__new__() takes from 0 to 1 positional arguments`) | Observed | Actors in tests and spikes take one settings object |
| `clock.set_timer_ns(name, interval_ns, start_time_ns=None, stop_time_ns=None, callback=None, …)`; with `callback=None`, events arrive at `on_time_event`. Timers fire in simulated time in backtests | Read: the stub, `docs/concepts/actors.md`. Observed: the spikes | The in-loop progress actor |
| `BacktestEngine` exposes `cache`, `portfolio` and `list_venues()`. `cache.account_for_venue(v).balances_total()`, `portfolio.total_pnls(v)`, `cache.positions_open_count()`, `cache.orders_total_count()` | Observed: the 2.0 stub, and the driver's snapshots | Snapshots between windows |
| `add_builtin_strategy("EmaCross", EmaCrossConfig(...))` runs a Rust EMA cross on quote mid prices | Observed | Tests and examples |
| Tearsheets: `nautilus_trader.analysis.tearsheet.create_tearsheet(engine, output_path=…)`, and `create_tearsheet_from_stats(stats_pnls, stats_returns, stats_general, returns, …)` with no engine; Plotly, from the `visualization` extra | Read: `docs/concepts/visualization.md`. Observed: the functions in the wheel | `gongoozler.nautilus.tearsheet` |
| External egress is non-blocking: "This outbound call must not block the node's bus thread. Bounded egress implementations drop on a full queue instead of applying back-pressure to the trading loop." Redis is the built-in backing; upstream installs it on live nodes (`LiveNodeBuilder::with_external_msgbus_factory`) | Read: committed `docs/concepts/message_bus.md` at `8b283ac` | The emitter's rule; a later Rust egress |
| `ParquetDataCatalog` works on object storage (`s3`, `gs`, `az`/`abfs`) through `storage_options`. `BacktestNode` streams a catalog in chunks: "Set `chunk_size` to a value in `[1, 1_000_000]` to enable streaming" | Read: `docs/concepts/data/catalog.md`, `apis-and-runs.md` | Catalog feeds |
| `ParquetDataCatalog(path)` writes by type: `write_quote_ticks`, `write_trade_ticks`, `write_bars`, `write_order_book_deltas`, `write_order_book_depths`, `write_mark_price_updates`, `write_index_price_updates`, `write_option_greeks`, `write_instrument_statuses`, `write_instrument_closes`, `write_instruments`; one Parquet file a call, named by its interval | Observed: the 2.0 wheel | `gongoozler.catalog.write` |
| **A catalog refuses files whose intervals overlap**, even at one shared timestamp: "would create non-disjoint intervals". So a feed's writes never split a timestamp | Observed | `catalog.write` |
| `BacktestDataConfig(data_type=NautilusDataType.QuoteTick, catalog=DataCatalogConfig(path=…), instrument_id=… \| instrument_ids=[…] \| bar_types=[…])`; `BacktestRunConfig(venues, data, engine, chunk_size, dispose_on_completion=False)`; `BacktestNode([rc]).build()`, then `add_builtin_strategy(rc.id, …)`, `add_actor(rc.id, actor)`, `get_engine_cache(rc.id)`, `get_engine_portfolio(rc.id)`, `run()` (a list of `BacktestResult`), `dispose()` | Observed | `gongoozler.nautilus.run_node` |
| `BacktestResult` carries `stats_pnls`, `stats_returns`, `stats_general`, `returns_series`, `total_orders`, `total_positions`; `engine.get_result()` gives it for an engine | Observed | `stats.json`, `returns.jsonl` |
| `to_dict()` of orders, fills, positions and account states carries fields that differ between identical runs: `init_id`, `event_id`, `causation_id` (random UUIDs), and nested event lists. With those left out, two identical runs give byte-identical records, `trade_id`s included | Observed: `test_the_same_run_in_another_process_writes_the_same_bytes` | `gongoozler.nautilus.UNSTABLE` |
| `account.events` is a list; `order.events` is a method | Observed | `_events_of` takes either |
| `engine.cache.instrument(InstrumentId)` is `None` for an instrument not added | Observed | Checking a feed's declared instruments |
| 2.0's `BacktestEngine` has no `trader` attribute; the `generate_*_report` functions are `BacktestNode`'s. gongoozler writes results from `to_dict()` instead, as canonical JSON | Observed | spec/results.md |

## The engine's Rust crates (`engine/`, nautilus-model and nautilus-analysis 0.67)

| Fact | Source | Used by |
| --- | --- | --- |
| `FuturesContract::builder()…build()` (a `bon` builder) validates its fields: `price_precision` must equal `price_increment`'s, the multiplier must be positive, strings ASCII; it returns an error, not a panic | Read: `instruments/futures_contract.rs`. Observed: a zero multiplier refused (`test_a_future_follows_its_nautilus_terms`) | `ws.future`, `fi_exec::ExchangeFuture` |
| The `Instrument` trait gives `multiplier()` (a `Quantity`), `price_increment()` (a `Price`), `activation_ns()` and `expiration_ns()` (`Option<UnixNanos>`) | Read: `instruments/mod.rs` | The future's margin, tick and listing window |
| `Price: FromStr` parses `"0.015625"` with the precision of its decimals (6) | Observed | `ws.future(price_increment=…)` |
| `Currency::try_from_str(code)` is `None` for a code the registry lacks; a currency's `precision` is its minor unit's decimals (ISO 4217 for fiat) | Read: `types/currency.rs`. Observed: `XYZ` refused | `fi_exec::minor_unit`, `ws.future(currency=…)` |
| `OrderBook` (L2) fills the engine's order-book fills; `nautilus-analysis`'s statistics (`SharpeRatio`, `SortinoRatio`, `MaxDrawdown`, `CAGR`, …) take returns keyed by `UnixNanos` | Observed: the engine's tests | `fi_exec::BookFill`, `fi-metrics` |

**Not upstream:** Ashley's SSE backing for the message bus is uncommitted work on the clone's local
branch `sse-msgbus-egress`, and its docs section is that branch's. Nothing here depends on it.
