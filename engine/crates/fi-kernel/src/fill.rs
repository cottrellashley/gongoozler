//! Fill models: how an order becomes a trade, at a price per unit.
//!
//! Prices are per unit, in the unit's own value terms, so one rule books every trade:
//! cash moves by −quantity × price, and the cost is quantity × (price − valuation). An unfunded
//! contract valued at zero (a future after settlement) therefore fills at its cost alone.

use crate::{entity::EntityId, time::Ts};

/// An order for a signed quantity of units.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Order {
    /// The contract.
    pub entity: EntityId,
    /// Signed quantity: positive buys.
    pub qty: f64,
    /// When.
    pub t: Ts,
}

/// A trade.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Fill {
    /// Signed quantity filled (may be less than ordered).
    pub qty: f64,
    /// Price per unit.
    pub price: f64,
    /// When the trade settles, where its quote stipulates it. Otherwise the book's settlement
    /// lag applies.
    pub settle_at: Option<Ts>,
}

impl Fill {
    /// A trade settling on the book's terms.
    #[must_use]
    pub fn new(qty: f64, price: f64) -> Self {
        Self { qty, price, settle_at: None }
    }
}

/// Turns orders into trades.
pub trait FillModel: Send + Sync {
    /// A short description (part of a run's fingerprint).
    fn name(&self) -> String;
    /// Fills `order`, given the unit's valuation at the order time.
    fn fill(&self, order: &Order, value: f64) -> Fill;
}

/// Research fills: everything fills at the valuation, plus a half spread in basis points of the
/// absolute value. With marks at bid and a full spread on buys, buys pay the spread and sells are
/// free, as the legacy frameworks charge it.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct ResearchFill {
    /// Half spread, in basis points of the unit's absolute value.
    pub half_spread_bp: f64,
}

impl FillModel for ResearchFill {
    fn name(&self) -> String {
        format!("research(half_spread_bp={})", self.half_spread_bp)
    }

    fn fill(&self, order: &Order, value: f64) -> Fill {
        Fill::new(order.qty, value + order.qty.signum() * value.abs() * self.half_spread_bp * 1e-4)
    }
}
