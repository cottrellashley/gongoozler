//! The order of a step's phases: declared, checked, and part of the run's settings, so its
//! manifest records it (review H3).
//!
//! A step always starts with what the held units paid and earned over the interval (contract
//! events, then interest), since nothing at the new time can be valued before that, and always
//! ends with the mark: NAV, checked against the identity. Between the two, the phases run in the
//! book's declared order. Events that share a time resolve in that order, so the order is an
//! assumption of the run, not a fact of the data: the platform brief asks for it to be documented,
//! and for economically material alternatives to be tested. A book tests one by declaring another
//! order.

use std::fmt;
use std::str::FromStr;

use crate::{Error, Result};

/// A phase between the contract events and the mark.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Phase {
    /// Legacy weight resets, when the holding policy asks for them.
    WeightResets,
    /// Subscriptions and redemptions.
    CapitalFlows,
    /// Each scheduled policy decides, by priority; intentions due now become working orders.
    Decisions,
    /// The execution model works the workable orders; fills book trades.
    Execution,
    /// Receivables and payables due now move into cash.
    Settlement,
    /// Financing recalls.
    Recalls,
    /// The distributor's payments.
    Distribution,
    /// Financing agreements: valuation, margin calls and returns.
    Agreements,
    /// The cash policy's response to obligations cash cannot meet.
    CashGuard,
    /// The mandate's constraints.
    Constraints,
}

impl Phase {
    /// Every phase, in the default order.
    pub const ALL: [Self; 10] = [
        Self::WeightResets,
        Self::CapitalFlows,
        Self::Decisions,
        Self::Execution,
        Self::Settlement,
        Self::Recalls,
        Self::Distribution,
        Self::Agreements,
        Self::CashGuard,
        Self::Constraints,
    ];

    /// The phase's name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::WeightResets => "weight_resets",
            Self::CapitalFlows => "capital_flows",
            Self::Decisions => "decisions",
            Self::Execution => "execution",
            Self::Settlement => "settlement",
            Self::Recalls => "recalls",
            Self::Distribution => "distribution",
            Self::Agreements => "agreements",
            Self::CashGuard => "cash_guard",
            Self::Constraints => "constraints",
        }
    }
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Phase {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|p| p.name() == s)
            .ok_or_else(|| Error::Invalid(format!("no phase {s:?} ({})", Self::ALL.iter().map(|p| p.name()).collect::<Vec<_>>().join(", "))))
    }
}

/// The order a book's steps run their phases in, between the contract events and the mark.
///
/// The default:
///
/// 1. weight resets (a legacy parity convention);
/// 2. capital flows: a redemption due now is known before anything is decided;
/// 3. decisions, then the intentions due now become orders; decisions see the NAV before this
///    step's capital flows, wherever those run;
/// 4. execution: a sale's proceeds can fund a purchase in the same batch;
/// 5. settlement: what is due now, including trades settling today, moves into cash after the
///    fills, so a purchase made now cannot spend cash that only arrives now;
/// 6. recalls, distribution and agreements: a margin call is computed on the holdings after
///    today's trades;
/// 7. the cash guard and the constraints, on everything above.
#[derive(Clone, PartialEq, Eq)]
pub struct StepOrder(Vec<Phase>);

impl Default for StepOrder {
    fn default() -> Self {
        Self(Phase::ALL.to_vec())
    }
}

impl fmt::Debug for StepOrder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "StepOrder({})", self.0.iter().map(|p| p.name()).collect::<Vec<_>>().join(" → "))
    }
}

impl StepOrder {
    /// An order of `phases`: every phase once, decisions before execution (an order's execution
    /// works what its decisions submitted; to execute a step later, a policy declares a lag).
    ///
    /// # Errors
    /// A phase missing or repeated, or execution before decisions.
    pub fn new(phases: Vec<Phase>) -> Result<Self> {
        for p in Phase::ALL {
            let n = phases.iter().filter(|x| **x == p).count();
            if n != 1 {
                return Err(Error::Invalid(format!("a step order lists every phase once: {p} appears {n} times")));
            }
        }
        if phases.len() != Phase::ALL.len() {
            return Err(Error::Invalid(format!("a step order has {} phases, not {}", Phase::ALL.len(), phases.len())));
        }
        let at = |p: Phase| phases.iter().position(|x| *x == p).unwrap_or(usize::MAX);
        if at(Phase::Execution) < at(Phase::Decisions) {
            return Err(Error::Invalid("execution must follow decisions (a policy's lag delays its orders)".into()));
        }
        Ok(Self(phases))
    }

    /// The phases, in order.
    #[must_use]
    pub fn phases(&self) -> &[Phase] {
        &self.0
    }

    /// The phases before execution and after it. Books run together share one execution, so
    /// each runs its own phases up to it, then the rest.
    #[must_use]
    pub fn around_execution(&self) -> (Vec<Phase>, Vec<Phase>) {
        let x = self.0.iter().position(|p| *p == Phase::Execution).unwrap_or(self.0.len());
        (self.0[..x].to_vec(), self.0[x + 1..].to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_order_lists_every_phase_once_with_execution_after_decisions() {
        assert_eq!(StepOrder::default().phases(), &Phase::ALL);
        let mut swapped = Phase::ALL.to_vec();
        swapped.swap(2, 3);
        assert!(StepOrder::new(swapped).is_err());
        let mut short = Phase::ALL.to_vec();
        short.pop();
        assert!(StepOrder::new(short).is_err());
        let mut twice = Phase::ALL.to_vec();
        twice[0] = Phase::Settlement;
        assert!(StepOrder::new(twice).is_err());
        // Settlement before the decisions: a legitimate alternative.
        let early = StepOrder::new(vec![
            Phase::WeightResets,
            Phase::Settlement,
            Phase::CapitalFlows,
            Phase::Decisions,
            Phase::Execution,
            Phase::Recalls,
            Phase::Distribution,
            Phase::Agreements,
            Phase::CashGuard,
            Phase::Constraints,
        ])
        .unwrap();
        let (before, after) = early.around_execution();
        assert_eq!(before.len(), 4);
        assert_eq!(after.len(), 5);
        assert_eq!("cash_guard".parse::<Phase>().unwrap(), Phase::CashGuard);
        assert!("lunch".parse::<Phase>().is_err());
    }
}
