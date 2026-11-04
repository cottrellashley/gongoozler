//! Interest rate swaps from two legs of terms: one leg received and one paid on the same
//! notional, so their principal exchanges cancel. Each leg is a debt's terms (fixed, stepped,
//! floating in advance or compounded in arrears, inflation-linked), stepped as a [`DebtContract`]
//! steps it and valued off the same curve. One unit is one unit of notional: its value is the
//! received leg's less the paid leg's, its flows and P&L the same difference, its P&L explained the
//! same way where both legs explain theirs. It is unfunded, so paying the received leg (a negative
//! quantity) needs no borrowed security.

use fi_kernel::{Contract, EntityId, Explain, Flows, Result, StepCtx, Stepper, Terms, Ts, UnitStep, ValuationState, Workspace, hash::Hash64};

use crate::DebtContract;

/// A swap: `receive` less `pay`.
#[derive(Clone, Debug)]
pub struct SwapContract {
    /// The swap.
    pub entity: EntityId,
    /// The leg received.
    pub receive: DebtContract,
    /// The leg paid.
    pub pay: DebtContract,
}

impl Contract for SwapContract {
    fn entity(&self) -> EntityId {
        self.entity
    }

    fn kind(&self) -> &'static str {
        "swap"
    }

    fn params(&self) -> u64 {
        let mut h = Hash64::new();
        h.str("swap");
        h.u64(self.receive.params());
        h.u64(self.pay.params());
        h.finish()
    }

    fn bind<'a>(&'a self, ws: &'a Workspace) -> Result<Box<dyn Stepper + 'a>> {
        Ok(Box::new(SwapStep { receive: self.receive.bind(ws)?, pay: self.pay.bind(ws)? }))
    }

    fn needs_borrow(&self) -> bool {
        false
    }

    fn reads(&self) -> Vec<String> {
        let mut r = self.receive.reads();
        r.extend(self.pay.reads());
        r.sort();
        r.dedup();
        r
    }

    fn marks(&self) -> Vec<String> {
        let mut r = self.receive.marks();
        r.extend(self.pay.marks());
        r.sort();
        r.dedup();
        r
    }

    fn supports(&self) -> Vec<&'static str> {
        let mut s = vec!["swap", "netted legs"];
        s.extend(self.receive.supports());
        s.extend(self.pay.supports());
        s.sort_unstable();
        s.dedup();
        s
    }
}

struct SwapStep<'a> {
    receive: Box<dyn Stepper + 'a>,
    pay: Box<dyn Stepper + 'a>,
}

fn state(a: ValuationState, b: ValuationState) -> ValuationState {
    use ValuationState::{Matured, Missing, Pending, Stale, Valid};
    match (a, b) {
        (Missing, _) | (_, Missing) => Missing,
        (Pending, Pending) => Pending,
        (Matured, Matured) => Matured,
        (Stale, _) | (_, Stale) => Stale,
        _ => Valid,
    }
}

fn net(r: UnitStep, p: UnitStep, ctx: &StepCtx<'_>) -> UnitStep {
    let mut flows = Flows::default();
    for i in 0..flows.cash.len() {
        flows.cash[i] = r.flows.cash[i] - p.flows.cash[i];
        flows.in_kind[i] = r.flows.in_kind[i] - p.flows.in_kind[i];
    }
    let terms = Terms {
        income: r.terms.income - p.terms.income,
        price: r.terms.price - p.terms.price,
        principal: r.terms.principal - p.terms.principal,
        other: r.terms.other - p.terms.other,
        financing: r.terms.financing - p.terms.financing,
    };
    // A leg's cash entitled now and paid later: the legs' netted where they pay together.
    let deferred = match (r.deferred, p.deferred) {
        (None, None) => None,
        (Some(d), None) => Some(d),
        (None, Some(d)) => Some(fi_kernel::Deferred { pay: d.pay, cash: -d.cash, income: -d.income }),
        (Some(a), Some(b)) if a.pay == b.pay => Some(fi_kernel::Deferred { pay: a.pay, cash: a.cash - b.cash, income: a.income - b.income }),
        (Some(a), Some(_)) => {
            ctx.fail(format!("a swap's legs defer cash to different dates (one to {})", a.pay));
            None
        }
    };
    let explain = match (r.explain, p.explain) {
        (Some(a), Some(b)) => Some(Explain { carry: a.carry - b.carry, curve: a.curve - b.curve, spread: a.spread - b.spread }),
        _ => None,
    };
    UnitStep { state: state(r.state, p.state), known: r.known.max(p.known), value: r.value - p.value, flows, terms, face: f64::NAN, deferred, explain }
}

impl Stepper for SwapStep<'_> {
    fn init(&mut self, t0: Ts, ctx: &StepCtx<'_>) -> UnitStep {
        let (r, p) = (self.receive.init(t0, ctx), self.pay.init(t0, ctx));
        net(r, p, ctx)
    }

    fn step(&mut self, from: Ts, to: Ts, ctx: &StepCtx<'_>) -> UnitStep {
        let (r, p) = (self.receive.step(from, to, ctx), self.pay.step(from, to, ctx));
        net(r, p, ctx)
    }
}
