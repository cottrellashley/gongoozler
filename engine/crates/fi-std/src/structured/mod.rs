//! Structured products: a deal is a book whose liabilities are contracts (spec, change 3).
//!
//! A [`Deal`] is a batch model. It runs its collateral as a book on the kernel's ledger: the
//! opening pool, the manager's policies and execution, and interest and principal collected
//! into separate accounts. A [`Waterfall`] is that book's distributor. It is the deal's own
//! priority of payments: two lists of steps, over the interest and the principal account, run
//! in order on each payment date. The engine imposes no order of its own. The waterfall's state
//! is held in the book's state, so a checkpoint holds it: tests, triggers, deferred interest, the
//! reserve and the deficiency ledgers.
//!
//! The deal writes its tranches' flows and marks as series. A tranche is then a
//! [`SuppliedBond`](crate::SuppliedBond) on them, held by any book like any other bond.
//!
//! * `{deal}.flows`: per tranche and payment date, per unit of original face: `interest`,
//!   `principal_face`, `writedown_face`, `writeup_face`, `pik_face`.
//! * `{deal}.marks`: per tranche and time:
//!   - `clean` and `accrued` per 100 of current face: debt at par with its accrued and deferred
//!     interest, equity at book value, an interest-only claim at zero;
//!   - also `factor`, `balance`, `deferred` and `deficiency`.
//! * `{deal}.distributions`: the cash paid to each tranche and fee on each payment date.
//! * `{deal}.tests`: each coverage test and trigger on each payment date: `value`, `threshold`,
//!   `passed`, `cure` (the cash diverted), `after` (the value after the cure) and `passed_after`.
//! * `{deal}.accounts`: the interest, principal and reserve accounts at each time.
//! * `{deal}.collateral`: the pool's `par` and `value`, the `interest` and `principal`
//!   collected, cumulative `losses`, the deal's `nav`, and whether it is `reinvesting`.
//! * `{deal}.claims`: the replacement notes a refinancing issued, each with the claim it
//!   `replaces` and its opening `balance`.

use std::collections::BTreeSet;
use std::sync::Arc;

use fi_kernel::{
    BatchModel, Calendar, Collected, Column, Contract, DecisionModel, Distribution, Distributor, DistributorRef, DistributorState, EntityId, Error, ExecRef,
    FillModel, Intervention, KnowledgeBasis, Ledger, LedgerConfig, MissingPolicy, ModelSpec, Policy, Producer, Provenance, Result, Rounding, Routing, Series,
    Shape, Ts, Unit, UnitRecords, Workspace, hash::Hash64,
};

mod coverage;
mod deal;
mod rights;
mod terms;
mod waterfall;

pub use terms::*;

const TINY: f64 = 1e-9;

const INTEREST: usize = 0;
const PRINCIPAL: usize = 1;
const WRITEDOWN: usize = 2;
const WRITEUP: usize = 3;
const PIK: usize = 4;

/// The deal's state: what its terms remember from one payment date to the next.
#[derive(Clone, Default)]
struct State {
    started: bool,
    ended: bool,
    balance: Vec<f64>,
    deferred: Vec<f64>,
    deficiency: Vec<f64>,
    written: Vec<f64>,
    issued: Vec<bool>,
    ever: Vec<bool>,
    successor: Vec<Option<usize>>,
    rate: Vec<f64>,
    due: Vec<f64>,
    fee_due: Vec<f64>,
    fee_carried: Vec<f64>,
    reserve: f64,
    tripped: Vec<bool>,
    observed: Vec<Vec<f64>>,
    start: Ts,
    par_start: f64,
    original_par: f64,
    period: Collected,
    period_loss: f64,
    cum_loss: f64,
    reinvest_until: Option<Ts>,
    default_event: bool,
    interest_start: f64,
    fees_paid: f64,
    pending: Vec<[f64; 5]>,
    paid: Vec<[f64; 3]>,
    out: Out,
}

#[derive(Clone, Default)]
struct Out {
    flows: Vec<(Ts, usize, [f64; 5])>,
    paid: Vec<(Ts, usize, [f64; 3])>,
    marks: Vec<[f64; 6]>,
    tests: Vec<(Ts, usize, [f64; 6])>,
    accounts: Vec<[f64; 3]>,
    collateral: Vec<[f64; 7]>,
    claims: Vec<(Ts, usize, usize, f64)>,
}

impl State {
    /// The claim outstanding in place of `j`: itself, or its replacement once redeemed.
    fn live(&self, mut j: usize) -> usize {
        while !self.issued[j] {
            match self.successor[j] {
                Some(n) if n != j => j = n,
                _ => break,
            }
        }
        j
    }

    /// The outstanding claims in place of `js`, once each, in order.
    fn outstanding(&self, js: &[usize]) -> Vec<usize> {
        let mut out: Vec<usize> = Vec::with_capacity(js.len());
        for &j in js {
            let j = self.live(j);
            if self.issued[j] && !out.contains(&j) {
                out.push(j);
            }
        }
        out
    }

    fn reference(&self, of: Notional, par: f64) -> f64 {
        match of {
            Notional::Pool => par,
            Notional::Tranche(i) => self.balance[self.live(i)],
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum Acct {
    Interest,
    Principal,
}

fn avail(d: &Distribution<'_>, a: Acct) -> f64 {
    match a {
        Acct::Interest => d.interest(),
        Acct::Principal => d.principal(),
    }
    .max(0.0)
}

fn ratio(num: f64, den: f64) -> f64 {
    if den > TINY { num / den } else { f64::INFINITY }
}

/// A deal's priority of payments over its collateral book: the book's distributor.
pub struct Waterfall {
    deal: Arc<Deal>,
}
