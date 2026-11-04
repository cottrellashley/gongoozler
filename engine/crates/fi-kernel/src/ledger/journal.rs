//! The journal: every transfer, with its cause (spec, change 7). Identifiers are stable: the
//! same run produces the same entries, so a retry or a restart duplicates nothing.

use crate::{entity::EntityId, time::Ts};

/// What an entry records.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum EntryKind {
    /// An order was submitted.
    Order,
    /// An order filled (in part or in full).
    Fill,
    /// An order was cancelled by a newer decision.
    Cancel,
    /// An order expired.
    Expire,
    /// A receivable or payable settled into cash.
    Settle,
    /// A contract's cash flow.
    Flow,
    /// A subscription or redemption.
    Capital,
    /// Interest on cash, or a borrow fee.
    Interest,
    /// A currency conversion.
    Conversion,
    /// A cash or short guard cut a trade, or a constraint was breached.
    Breach,
    /// A decision's note (an optimiser's status, a binding constraint).
    Note,
    /// The book failed economically: an obligation it could not meet, or NAV at or below zero.
    Failure,
    /// A sale at the horizon.
    Liquidation,
    /// A securities recall bought back.
    Recall,
    /// A distribution or allocation (a waterfall payment).
    Distribution,
    /// A financing agreement's transfer (a draw, a repayment, a margin call, collateral).
    Financing,
    /// A holding transition: units of one contract leaving, units of another or cash arriving.
    Transition,
}

/// Why an entry happened.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Cause {
    /// A policy's decision.
    Policy(usize),
    /// An order.
    Order(u64),
    /// A contract's lifecycle.
    Contract(EntityId),
    /// A dated receivable or payable.
    Due(u64),
    /// External capital.
    Capital,
    /// Interest or fees.
    Interest,
    /// The horizon policy.
    Horizon,
    /// A financing agreement.
    Agreement(usize),
    /// The book's own rules (guards, failure).
    Book,
    /// A holding transition (its position in the records' list).
    Transition(usize),
}

/// One journal entry: the brief's event envelope.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// Stable identifier.
    pub id: u64,
    /// Effective time.
    pub t: Ts,
    /// What it records.
    pub kind: EntryKind,
    /// The subject: a contract, a currency, or `EntityId::NONE`.
    pub entity: EntityId,
    /// The currency of `amount` (`EntityId::NONE`: the base currency).
    pub currency: EntityId,
    /// Units (trades, recalls).
    pub qty: f64,
    /// Amount, in `currency`.
    pub amount: f64,
    /// Why.
    pub cause: Cause,
    /// A note, where there is one.
    pub note: Option<String>,
}
