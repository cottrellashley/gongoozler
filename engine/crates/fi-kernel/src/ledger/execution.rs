//! Execution as a process (spec, change 2): orders persist, are worked over time, and fill in
//! part, in full or not at all.

use std::sync::Arc;

use crate::{
    entity::EntityId,
    fill::{Fill, FillModel, Order},
    time::Ts,
};

/// An order the book is working.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkingOrder {
    /// Stable identifier.
    pub id: u64,
    /// The contract.
    pub entity: EntityId,
    /// Its record row.
    pub row: usize,
    /// Units still to trade (signed: positive buys).
    pub remaining: f64,
    /// Units asked for.
    pub requested: f64,
    /// Units filled so far.
    pub filled: f64,
    /// Σ filled units × price, in the contract's currency.
    pub paid: f64,
    /// Submission time.
    pub submitted: Ts,
    /// Submission step.
    pub submitted_k: usize,
    /// First step it can be worked (after the execution model's latency).
    pub workable_k: usize,
    /// Last step it works.
    pub expires_k: usize,
    /// Attempts so far.
    pub attempts: u32,
    /// The policy that asked for it.
    pub policy: usize,
    /// Orders submitted together by one policy share a basket.
    pub basket: u64,
    /// The unit's value when it was decided.
    pub decision_value: f64,
}

/// How orders execute. A model sees every workable order of a step together, so baskets can be
/// priced as baskets and capacity shared; randomness is keyed by order and attempt, never by
/// worker.
pub trait Execution: Send + Sync {
    /// Name and parameters.
    fn name(&self) -> String;
    /// Steps between submission and the first attempt.
    fn latency(&self) -> usize {
        0
    }
    /// One fill per order, in order (a zero quantity: nothing filled this step). `values[j]` is
    /// order `j`'s unit value now.
    fn work(&self, t: Ts, orders: &[WorkingOrder], values: &[f64]) -> Vec<Fill>;
}

/// How an order ended.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum OrderStatus {
    /// Filled in full.
    Filled,
    /// Expired after filling in part.
    Partial,
    /// Expired with nothing filled.
    Expired,
    /// Replaced by a newer decision of its policy.
    Cancelled,
    /// Still working when the run ended.
    Open,
}

/// One order's life (kept with the journal).
#[derive(Clone, Debug, PartialEq)]
pub struct OrderRecord {
    /// Identifier.
    pub id: u64,
    /// The contract.
    pub entity: EntityId,
    /// The policy that asked for it.
    pub policy: usize,
    /// Submitted.
    pub submitted: Ts,
    /// Ended (filled, expired or cancelled).
    pub ended: Option<Ts>,
    /// Units asked for.
    pub requested: f64,
    /// Units filled.
    pub filled: f64,
    /// Average fill price.
    pub average_price: f64,
    /// The unit's value when decided.
    pub decision_value: f64,
    /// Attempts.
    pub attempts: u32,
    /// How it ended.
    pub status: OrderStatus,
}

/// What the run's orders did, in total.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OrderStats {
    /// Orders submitted.
    pub submitted: u64,
    /// Filled in full.
    pub filled: u64,
    /// Expired after a partial fill.
    pub partial: u64,
    /// Expired with nothing filled.
    pub expired: u64,
    /// Cancelled by a newer decision.
    pub cancelled: u64,
    /// Σ |units asked| × value at decision, in the base currency.
    pub requested_value: f64,
    /// Σ |units filled| × value at decision, in the base currency.
    pub filled_value: f64,
    /// Implementation shortfall from the decision to the fill: Σ fill × (value at fill − value
    /// at decision), in the base currency (positive is a cost).
    pub delay_cost: f64,
    /// What the unfilled part would have made from decision to expiry: Σ unfilled × (value at
    /// expiry − value at decision), in the base currency.
    pub missed: f64,
}

/// A fill model as an execution process: each order fills on its own, at once or not at all.
pub struct EachOrder(pub Arc<dyn FillModel>);

impl Execution for EachOrder {
    fn name(&self) -> String {
        self.0.name()
    }

    fn work(&self, t: Ts, orders: &[WorkingOrder], values: &[f64]) -> Vec<Fill> {
        orders.iter().zip(values).map(|(o, v)| self.0.fill(&Order { entity: o.entity, qty: o.remaining, t }, *v)).collect()
    }
}
