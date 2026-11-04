//! Execution on Nautilus's crates, used as libraries: no fork, no Python.
//!
//! * [`BookFill`]: fills against a simulated L2 order book, Nautilus's
//!   `nautilus_model::orderbook::OrderBook`: exchange-traded hedges, with depth, partial fills
//!   and slippage.
//! * [`RfqFill`]: OTC request for quote. A panel of dealers quote around the valuation; the best
//!   quote wins; dealers may decline; sizes are capped. Deterministic for a given seed.
//! * [`QuoteFill`]: recorded firm quotes, from a series. A bid fills a sale and an ask a
//!   purchase, only at the quote's own instant and up to its size; a quote may stipulate when its
//!   trades settle.
//! * [`minor_unit`]: a currency's smallest unit, from Nautilus's currency registry (ISO 4217 for
//!   fiat currencies), for contracts whose rounding is in whole minor units.
//!
//! Both implement the kernel's [`FillModel`], so the same strategy runs at research fidelity or
//! with these models, and the ledger books the difference as cost.

pub mod futures;

pub use futures::{ExchangeFuture, Roll};

use std::collections::HashMap;
use std::sync::Arc;

use fi_kernel::{EntityId, Error, Execution, Fill, FillModel, Order, Result, Rounding, RoundingMode, Series, Ts, WorkingOrder, Workspace, hash::Hash64};
use nautilus_core::UnixNanos;
use nautilus_model::{
    data::order::BookOrder,
    enums::{BookType, OrderSide},
    identifiers::InstrumentId,
    orderbook::OrderBook,
    types::{Currency, Price, Quantity},
};

/// A currency's smallest unit (0.01 for USD, 1 for JPY), from Nautilus's currency registry: ISO
/// 4217's minor units for fiat currencies. `None` for a code the registry does not know.
#[must_use]
pub fn minor_unit(code: &str) -> Option<f64> {
    Currency::try_from_str(code).map(|c| 10f64.powi(-i32::from(c.precision)))
}

/// A rounding term in whole minor units of `code`, by `mode`.
///
/// # Errors
/// A currency code the registry does not know.
pub fn rounding(code: &str, mode: RoundingMode) -> Result<Rounding> {
    minor_unit(code).map(|unit| Rounding { unit, mode }).ok_or_else(|| Error::Unknown(format!("no currency {code} in Nautilus's currency registry")))
}

/// One book level: a price in the book's own quote units and a size in units.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Level {
    /// Price, in quote units (for example per 100, or index points).
    pub price: f64,
    /// Size, in contract units.
    pub size: f64,
}

struct Book {
    book: OrderBook,
    mark: f64,
    per_unit: f64,
}

/// Fills against Nautilus order books. A book quotes in its own units; a fill's distance from the
/// book's mark is converted to unit value with `per_unit` (for example, multiplier × points).
pub struct BookFill {
    books: HashMap<EntityId, Book>,
    price_precision: u8,
    size_precision: u8,
}

impl BookFill {
    /// No books yet.
    #[must_use]
    pub fn new(price_precision: u8, size_precision: u8) -> Self {
        Self { books: HashMap::new(), price_precision, size_precision }
    }

    /// Sets the book of `entity`: bids and asks (any order), the mark the unit's value is measured
    /// from, and the unit value of one quote point.
    pub fn set_book(&mut self, entity: EntityId, bids: &[Level], asks: &[Level], mark: f64, per_unit: f64) {
        let id = InstrumentId::from(format!("E{}.SIM", entity.0).as_str());
        let mut book = OrderBook::new(id, BookType::L2_MBP);
        let mut seq = 0u64;
        for (side, levels) in [(OrderSide::Buy, bids), (OrderSide::Sell, asks)] {
            for l in levels {
                seq += 1;
                let o = BookOrder::new(side, Price::new(l.price, self.price_precision), Quantity::new(l.size, self.size_precision), seq);
                book.add(o, 0, seq, UnixNanos::from(0u64));
            }
        }
        self.books.insert(entity, Book { book, mark, per_unit });
    }

    /// Best bid and ask of `entity`'s book.
    #[must_use]
    pub fn top(&self, entity: EntityId) -> Option<(f64, f64)> {
        let b = &self.books.get(&entity)?.book;
        Some((b.best_bid_price()?.as_f64(), b.best_ask_price()?.as_f64()))
    }
}

impl FillModel for BookFill {
    fn name(&self) -> String {
        format!("book(books={})", self.books.len())
    }

    fn fill(&self, order: &Order, value: f64) -> Fill {
        let Some(b) = self.books.get(&order.entity) else {
            return Fill::new(0.0, value);
        };
        let (side, limit) = if order.qty > 0.0 { (OrderSide::Buy, 1e9) } else { (OrderSide::Sell, 0.0) };
        let probe = BookOrder::new(side, Price::new(limit, self.price_precision), Quantity::new(order.qty.abs(), self.size_precision), 0);
        let fills = b.book.simulate_fills(&probe);
        let filled: f64 = fills.iter().map(|(_, q)| q.as_f64()).sum();
        if filled <= 0.0 {
            return Fill::new(0.0, value);
        }
        let avg = fills.iter().map(|(p, q)| p.as_f64() * q.as_f64()).sum::<f64>() / filled;
        Fill::new(filled.copysign(order.qty), value + (avg - b.mark) * b.per_unit)
    }
}

/// OTC request for quote across a dealer panel.
#[derive(Clone, Debug)]
pub struct RfqFill {
    /// Dealers asked.
    pub dealers: usize,
    /// Each dealer's chance of quoting.
    pub response: f64,
    /// Quoted half spread, in basis points of the unit's value.
    pub half_spread_bp: f64,
    /// Dispersion of quotes around it, in basis points.
    pub dispersion_bp: f64,
    /// Largest size one request fills, in units.
    pub max_size: f64,
    /// Seed: the same seed gives the same quotes.
    pub seed: u64,
}

fn unit(h: u64) -> f64 {
    ((h >> 11) as f64 + 0.5) / (1u64 << 53) as f64
}

impl FillModel for RfqFill {
    fn name(&self) -> String {
        format!(
            "rfq(dealers={}, response={}, half_spread_bp={}, dispersion_bp={}, max_size={})",
            self.dealers, self.response, self.half_spread_bp, self.dispersion_bp, self.max_size
        )
    }

    fn fill(&self, order: &Order, value: f64) -> Fill {
        let buy = order.qty > 0.0;
        let mut best: Option<f64> = None;
        for d in 0..self.dealers {
            let mut h = Hash64::new();
            h.u64(self.seed);
            h.u64(u64::from(order.entity.0));
            h.i64(order.t.0);
            h.u64(d as u64);
            h.u64(u64::from(buy));
            let r = h.finish();
            if unit(r) > self.response {
                continue;
            }
            let mut h2 = Hash64::new();
            h2.u64(r);
            let bp = self.half_spread_bp + self.dispersion_bp * unit(h2.finish());
            let quote = value + if buy { 1.0 } else { -1.0 } * value.abs() * bp * 1e-4;
            best = Some(match best {
                None => quote,
                Some(b) if buy => b.min(quote),
                Some(b) => b.max(quote),
            });
        }
        match best {
            Some(price) => Fill::new(order.qty.abs().min(self.max_size).copysign(order.qty), price),
            None => Fill::new(0.0, value),
        }
    }
}

/// Request for quote as a process (spec, change 2). Each step an order works, the panel is asked
/// again: a request nobody answers fills nothing and the order keeps working until it expires; a
/// size cap fills part of it per attempt. The first attempt comes `latency` steps after the
/// decision's submission. Draws are keyed by order and time, so retries are independent and
/// reproducible.
#[derive(Clone, Debug)]
pub struct RfqProcess {
    /// The dealer panel.
    pub panel: RfqFill,
    /// Steps from submission to the first request.
    pub latency: usize,
}

impl Execution for RfqProcess {
    fn name(&self) -> String {
        format!("rfq_process(latency={}, {})", self.latency, self.panel.name())
    }

    fn latency(&self) -> usize {
        self.latency
    }

    fn work(&self, t: Ts, orders: &[WorkingOrder], values: &[f64]) -> Vec<Fill> {
        orders
            .iter()
            .zip(values)
            .map(|(o, v)| {
                let mut h = Hash64::new();
                h.u64(self.panel.seed);
                h.u64(o.id);
                h.u64(u64::from(o.attempts));
                let panel = RfqFill { seed: h.finish(), ..self.panel.clone() };
                panel.fill(&Order { entity: o.entity, qty: o.remaining, t }, *v)
            })
            .collect()
    }
}

/// A portfolio trade: the orders a policy submits together are quoted as one basket, not as a sum
/// of single-bond trades. The basket's dealer quotes one spread for all of it,
/// `half_spread_bp + impact_bp × √(notional ÷ 1,000,000)`, and accepts the whole basket with
/// probability `acceptance`; a rejected basket fills nothing this step and its orders keep
/// working until they expire.
#[derive(Clone, Debug)]
pub struct PortfolioTrade {
    /// The basket's base half spread, in basis points.
    pub half_spread_bp: f64,
    /// Extra cost per √(million of notional), in basis points.
    pub impact_bp: f64,
    /// The chance a dealer takes the basket.
    pub acceptance: f64,
    /// Seed.
    pub seed: u64,
    /// Steps from submission to the first request.
    pub latency: usize,
}

impl Execution for PortfolioTrade {
    fn name(&self) -> String {
        format!("portfolio(half_spread_bp={}, impact_bp={}, acceptance={}, latency={})", self.half_spread_bp, self.impact_bp, self.acceptance, self.latency)
    }

    fn latency(&self) -> usize {
        self.latency
    }

    fn work(&self, _t: Ts, orders: &[WorkingOrder], values: &[f64]) -> Vec<Fill> {
        let mut notional: HashMap<u64, f64> = HashMap::new();
        let mut attempt: HashMap<u64, u32> = HashMap::new();
        for (o, v) in orders.iter().zip(values) {
            *notional.entry(o.basket).or_default() += (o.remaining * v).abs();
            let a = attempt.entry(o.basket).or_default();
            *a = (*a).max(o.attempts);
        }
        let accepted: HashMap<u64, bool> = attempt
            .iter()
            .map(|(b, a)| {
                let mut h = Hash64::new();
                h.u64(self.seed);
                h.u64(*b);
                h.u64(u64::from(*a));
                (*b, unit(h.finish()) <= self.acceptance)
            })
            .collect();
        orders
            .iter()
            .zip(values)
            .map(|(o, v)| {
                if !accepted[&o.basket] {
                    return Fill::new(0.0, *v);
                }
                let bp = self.half_spread_bp + self.impact_bp * (notional[&o.basket] / 1e6).sqrt();
                let side = if o.remaining > 0.0 { 1.0 } else { -1.0 };
                Fill::new(o.remaining, v + side * v.abs() * bp * 1e-4)
            })
            .collect()
    }
}

/// Capacity: at most `max_value` (in the contracts' currency) per contract per step, across every
/// order of every book run together, allocated pro rata to what each order filled.
pub struct Capacity {
    /// The execution being limited.
    pub inner: Arc<dyn Execution>,
    /// The most that trades in one contract per step.
    pub max_value: f64,
}

impl Execution for Capacity {
    fn name(&self) -> String {
        format!("capacity(max_value={}, {})", self.max_value, self.inner.name())
    }

    fn latency(&self) -> usize {
        self.inner.latency()
    }

    fn work(&self, t: Ts, orders: &[WorkingOrder], values: &[f64]) -> Vec<Fill> {
        let mut fills = self.inner.work(t, orders, values);
        let mut total: HashMap<EntityId, f64> = HashMap::new();
        for ((o, v), f) in orders.iter().zip(values).zip(&fills) {
            *total.entry(o.entity).or_default() += (f.qty * v).abs();
        }
        for ((o, _), f) in orders.iter().zip(values).zip(fills.iter_mut()) {
            let used = total[&o.entity];
            if used > self.max_value {
                f.qty *= self.max_value / used;
            }
        }
        fills
    }
}

/// Recorded firm quotes: a bid fills a sale and an ask a purchase, at exactly the quote's instant
/// (and only once it is known), up to its size, at its price per unit. Each quote may stipulate,
/// in seconds after it, when its trades settle. With no quote at an order's instant there is no
/// fill, and the order works on or expires under the book's terms.
#[derive(Clone)]
pub struct QuoteFill {
    quotes: Arc<Series>,
    bid: Option<(usize, usize)>,
    ask: Option<(usize, usize)>,
    settle_after: Option<usize>,
    label: String,
}

impl QuoteFill {
    /// Quotes from `series`: `bid` and `ask` name (price column, size column); `settle_after`
    /// names a column of seconds from the quote to its trades' settlement.
    ///
    /// # Errors
    /// An unknown series or column, or neither side given.
    pub fn new(ws: &Workspace, series: &str, bid: Option<(&str, &str)>, ask: Option<(&str, &str)>, settle_after: Option<&str>) -> Result<Self> {
        if bid.is_none() && ask.is_none() {
            return Err(Error::Invalid(format!("quotes from {series} need a bid or an ask")));
        }
        let quotes = ws.outcome(series)?.clone();
        let side = |s: Option<(&str, &str)>| -> Result<Option<(usize, usize)>> { s.map(|(p, q)| Ok((quotes.column(p)?, quotes.column(q)?))).transpose() };
        let (bid, ask) = (side(bid)?, side(ask)?);
        let settle_after = settle_after.map(|c| quotes.column(c)).transpose()?;
        Ok(Self { label: format!("{series}, {:016x}", quotes.version()), quotes, bid, ask, settle_after })
    }
}

impl FillModel for QuoteFill {
    fn name(&self) -> String {
        format!("quotes({})", self.label)
    }

    fn fill(&self, order: &Order, value: f64) -> Fill {
        let none = Fill::new(0.0, value);
        let Some((price, size)) = (if order.qty < 0.0 { self.bid } else { self.ask }) else { return none };
        let at = |c: usize| self.quotes.exact(c, order.entity, order.t, order.t).map(|h| h.value);
        let (Some(price), Some(size)) = (at(price), at(size)) else { return none };
        if !price.is_finite() || size.is_nan() || size <= 0.0 {
            return none;
        }
        let settle_at = self.settle_after.and_then(at).map(|secs| order.t.plus((secs * 1e9).round() as i64));
        Fill { qty: order.qty.abs().min(size).copysign(order.qty), price, settle_at }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fi_kernel::Ts;

    fn order(qty: f64) -> Order {
        Order { entity: EntityId(1), qty, t: Ts(0) }
    }

    #[test]
    fn minor_units_come_from_the_currency_registry() {
        assert_eq!(minor_unit("USD"), Some(0.01));
        assert_eq!(minor_unit("EUR"), Some(0.01));
        assert_eq!(minor_unit("JPY"), Some(1.0));
        // The registry holds the major currencies, not all of ISO 4217: for one it lacks (BHD has
        // three decimals), the contract states its unit.
        assert_eq!(minor_unit("BHD"), None);
        assert_eq!(minor_unit("XYZ"), None);
        let r = rounding("USD", RoundingMode::Down).unwrap();
        assert_eq!(r.round(950_000.009), 950_000.0);
    }

    #[test]
    fn quotes_fill_only_at_their_instant_and_up_to_their_size() {
        use fi_kernel::{Column, KnowledgePolicy, Unit};
        let mut ws = Workspace::new(KnowledgePolicy::AS_KNOWN);
        let b = ws.entities.intern("bond", "B");
        let (t1, t2) = (Ts::from_days(10), Ts::from_days(11));
        let cols = vec![
            Column::new("bid", Unit::Price, vec![0.99, 0.97]),
            Column::new("size", Unit::Number, vec![5_000.0, 300.0]),
            Column::new("settle", Unit::Number, vec![86_400.0, f64::NAN]),
        ];
        ws.add_series(Series::rows("quotes", vec![b, b], vec![t1, t2], vec![t1, t2], cols).unwrap()).unwrap();
        let q = QuoteFill::new(&ws, "quotes", Some(("bid", "size")), None, Some("settle")).unwrap();
        let sell = |qty: f64, t: Ts| q.fill(&Order { entity: b, qty, t }, 1.0);
        let first = sell(-1_000.0, t1);
        assert_eq!((first.qty, first.price, first.settle_at), (-1_000.0, 0.99, Some(t1.plus(Ts::DAY))));
        let capped = sell(-1_000.0, t2);
        assert_eq!((capped.qty, capped.price, capped.settle_at), (-300.0, 0.97, None), "capped at its size; the book's settlement");
        assert_eq!(sell(-1_000.0, t1.plus(1)).qty, 0.0, "a firm quote is firm only at its instant");
        assert_eq!(q.fill(&Order { entity: b, qty: 10.0, t: t1 }, 1.0).qty, 0.0, "no ask, no purchase");
    }

    #[test]
    fn book_fills_walk_the_depth() {
        let mut f = BookFill::new(4, 0);
        let asks = [Level { price: 100.1, size: 1000.0 }, Level { price: 100.2, size: 2000.0 }, Level { price: 100.5, size: 5000.0 }];
        let bids = [Level { price: 99.9, size: 1500.0 }, Level { price: 99.5, size: 3000.0 }];
        f.set_book(EntityId(1), &bids, &asks, 100.0, 1.0);
        assert_eq!(f.top(EntityId(1)), Some((99.9, 100.1)));
        let buy = f.fill(&order(2500.0), 0.0);
        assert_eq!(buy.qty, 2500.0);
        assert!((buy.price - (1000.0 * 100.1 + 1500.0 * 100.2) / 2500.0 + 100.0).abs() < 1e-9, "buy at {}", buy.price);
        let sell = f.fill(&order(-2000.0), 0.0);
        assert!((sell.price - ((1500.0 * 99.9 + 500.0 * 99.5) / 2000.0 - 100.0)).abs() < 1e-9);
        let too_big = f.fill(&order(10_000.0), 0.0);
        assert_eq!(too_big.qty, 8000.0, "a partial fill when the book runs out");
        assert_eq!(f.fill(&Order { entity: EntityId(9), qty: 1.0, t: Ts(0) }, 5.0).qty, 0.0, "no book, no fill");
    }

    fn working(id: u64, entity: u32, qty: f64, basket: u64) -> WorkingOrder {
        WorkingOrder {
            id,
            entity: EntityId(entity),
            row: entity as usize,
            remaining: qty,
            requested: qty,
            filled: 0.0,
            paid: 0.0,
            submitted: Ts(0),
            submitted_k: 0,
            workable_k: 0,
            expires_k: 0,
            attempts: 0,
            policy: 0,
            basket,
            decision_value: 1.0,
        }
    }

    #[test]
    fn a_basket_is_quoted_whole_and_capacity_is_shared() {
        let pt = PortfolioTrade { half_spread_bp: 2.0, impact_bp: 1.0, acceptance: 1.0, seed: 1, latency: 0 };
        let orders = [working(1, 1, 3e6, 7), working(2, 2, -1e6, 7)];
        let fills = pt.work(Ts(0), &orders, &[1.0, 1.0]);
        let bp = 2.0 + (4.0f64).sqrt();
        assert!((fills[0].price - (1.0 + bp * 1e-4)).abs() < 1e-12 && (fills[1].price - (1.0 - bp * 1e-4)).abs() < 1e-12);
        let never = PortfolioTrade { acceptance: 0.0, ..pt.clone() };
        assert!(never.work(Ts(0), &orders, &[1.0, 1.0]).iter().all(|f| f.qty == 0.0), "a rejected basket fills nothing");
        let cap = Capacity { inner: Arc::new(pt), max_value: 1e6 };
        let shared = [working(1, 1, 3e6, 7), working(2, 1, 1e6, 8)];
        let f = cap.work(Ts(0), &shared, &[1.0, 1.0]);
        assert!((f[0].qty - 0.75e6).abs() < 1e-6 && (f[1].qty - 0.25e6).abs() < 1e-6, "pro rata: {:?}", f);
    }

    #[test]
    fn rfq_is_deterministic_and_pays_at_least_the_half_spread() {
        let rfq = RfqFill { dealers: 5, response: 0.7, half_spread_bp: 3.0, dispersion_bp: 4.0, max_size: 2_000_000.0, seed: 11 };
        let a = rfq.fill(&order(1_000_000.0), 1.0);
        assert_eq!(a, rfq.fill(&order(1_000_000.0), 1.0));
        assert!(a.qty == 0.0 || a.price >= 1.0 + 3e-4 - 1e-15);
        let s = rfq.fill(&order(-5_000_000.0), 1.0);
        assert!(s.qty == 0.0 || (s.qty == -2_000_000.0 && s.price <= 1.0 - 3e-4 + 1e-15));
        let silent = RfqFill { response: 0.0, ..rfq };
        assert_eq!(silent.fill(&order(1.0), 1.0).qty, 0.0);
    }
}

/// A bid list, as a BWIC auctions one (review M5): each order is a line auctioned whole to
/// `bidders`, each bidding with probability `participation` at the unit's value less a discount
/// (for a sale; plus it, for a purchase) drawn between `discount_bp` and `discount_bp +
/// dispersion_bp`. The best bid trades the whole line if it clears the reserve (`reserve_bp`, the
/// most the book gives up); otherwise the line does not trade, and nothing fills. Draws are keyed
/// by the seed, the line and the time, so the same list gives the same outcome.
#[derive(Clone, Debug)]
pub struct AuctionFill {
    /// Bidders asked.
    pub bidders: usize,
    /// Each bidder's chance of bidding.
    pub participation: f64,
    /// The least discount a bid carries, in basis points of the unit's value.
    pub discount_bp: f64,
    /// The spread of bids beyond it, in basis points.
    pub dispersion_bp: f64,
    /// The most the book gives up below its value (above, for a purchase); infinite: no reserve.
    pub reserve_bp: f64,
    /// Seed.
    pub seed: u64,
}

impl FillModel for AuctionFill {
    fn name(&self) -> String {
        format!(
            "auction(bidders={}, participation={}, discount_bp={}, dispersion_bp={}, reserve_bp={})",
            self.bidders, self.participation, self.discount_bp, self.dispersion_bp, self.reserve_bp
        )
    }

    fn fill(&self, order: &Order, value: f64) -> Fill {
        let buy = order.qty > 0.0;
        let mut best: Option<f64> = None;
        for b in 0..self.bidders {
            let mut h = Hash64::new();
            h.str("auction");
            h.u64(self.seed);
            h.u64(u64::from(order.entity.0));
            h.i64(order.t.0);
            h.u64(b as u64);
            let r = h.finish();
            if unit(r) > self.participation {
                continue;
            }
            let mut h2 = Hash64::new();
            h2.u64(r);
            let bp = self.discount_bp + self.dispersion_bp * unit(h2.finish());
            best = Some(best.map_or(bp, |x: f64| x.min(bp)));
        }
        match best {
            Some(bp) if bp <= self.reserve_bp => {
                let price = value + if buy { 1.0 } else { -1.0 } * value.abs() * bp * 1e-4;
                Fill::new(order.qty, price)
            }
            _ => Fill::new(0.0, value),
        }
    }
}

/// Primary allocation (review M5): an order to buy a new issue is allocated a fraction of its
/// size at the issue's reoffer price (per 100 of face, a new issue carrying no accrued interest);
/// every other order fills through `otherwise`.
pub struct PrimaryFill {
    /// The new issues: (contract, reoffer per 100, fraction allocated).
    pub issues: Vec<(EntityId, f64, f64)>,
    /// How every other order fills.
    pub otherwise: Arc<dyn FillModel>,
}

impl FillModel for PrimaryFill {
    fn name(&self) -> String {
        let issues: Vec<String> = self.issues.iter().map(|(e, p, a)| format!("{}@{p}x{a}", e.0)).collect();
        format!("primary({}; otherwise {})", issues.join(","), self.otherwise.name())
    }

    fn fill(&self, order: &Order, value: f64) -> Fill {
        match self.issues.iter().find(|(e, _, _)| *e == order.entity) {
            Some((_, reoffer, allocation)) if order.qty > 0.0 => Fill::new(order.qty * allocation.clamp(0.0, 1.0), reoffer / 100.0),
            _ => self.otherwise.fill(order, value),
        }
    }
}

#[cfg(test)]
mod auction_tests {
    use super::*;

    #[test]
    fn a_bid_list_trades_lines_whole_or_not_at_all_and_never_below_its_reserve() {
        let a = AuctionFill { bidders: 4, participation: 0.6, discount_bp: 20.0, dispersion_bp: 30.0, reserve_bp: 35.0, seed: 9 };
        let (mut traded, mut dnt) = (0, 0);
        for k in 0..400 {
            let o = Order { entity: EntityId(k % 50), qty: -1_000_000.0, t: Ts::from_days(i64::from(k)) };
            let f = a.fill(&o, 0.98);
            if f.qty == 0.0 {
                dnt += 1;
            } else {
                traded += 1;
                assert_eq!(f.qty, o.qty, "a line trades whole");
                let bp = (0.98 - f.price) / 0.98 * 1e4;
                assert!((20.0 - 1e-9..=35.0 + 1e-9).contains(&bp), "{bp}bp");
            }
            assert_eq!(a.fill(&o, 0.98), f, "the same list, the same outcome");
        }
        assert!(traded > 100 && dnt > 50, "{traded} traded, {dnt} did not");
        let p = PrimaryFill { issues: vec![(EntityId(1), 99.5, 0.4)], otherwise: Arc::new(a) };
        let f = p.fill(&Order { entity: EntityId(1), qty: 1_000.0, t: Ts::from_days(1) }, f64::NAN);
        assert_eq!((f.qty, f.price), (400.0, 0.995));
    }
}
