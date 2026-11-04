//! Exchange-traded futures defined by Nautilus's `FuturesContract`, and rolling between them
//! (review H5; the brief's R4 and P9).
//!
//! A future's terms are Nautilus's instrument: its multiplier, tick, activation, expiration and
//! currency are what Nautilus's own checks accept, not numbers re-specified by hand. One unit is one
//! contract. It is marked to market each step: variation margin, the multiplier times the change in
//! the settlement price (on the tick), paid as a performance flow, so the unit's value is zero.
//! Before activation it does not trade (pending, no value); at expiration it settles a last time
//! and expires.
//!
//! A roll is a policy, never a series: [`Roll`] holds a number of contracts in the first contract
//! of a chain still more than `days_before` from expiry, and moves the position to the next one in
//! one decision. The ledger books the sale and the purchase like any trades, so the contract
//! change, the realised margin and the exposure reconcile through the same identity.

use fi_kernel::{
    Contract, DecisionCtx, DecisionModel, EntityId, Error, Flows, Result, Role, Series, StepCtx, Stepper, Targets, Terms, Ts, UnitStep, ValuationState,
    Workspace, hash::Hash64,
};
use nautilus_model::instruments::{FuturesContract, Instrument};

/// A future whose terms are a Nautilus `FuturesContract`.
#[derive(Clone, Debug)]
pub struct ExchangeFuture {
    /// The contract (an entity of the price series).
    pub entity: EntityId,
    /// Its terms.
    pub instrument: FuturesContract,
    /// Its settlement prices: (series, column).
    pub price: (String, String),
    /// Its currency in the book (an entity of kind `currency`; `None`: the book's base).
    pub currency: Option<EntityId>,
}

impl ExchangeFuture {
    fn multiplier(&self) -> f64 {
        self.instrument.multiplier().as_f64()
    }

    fn tick(&self) -> f64 {
        self.instrument.price_increment().as_f64()
    }

    /// Activation and expiration (open-ended where Nautilus has none).
    fn window(&self) -> (Ts, Ts) {
        let ts = |n: Option<nautilus_core::UnixNanos>, none: Ts| n.map_or(none, |x| Ts(x.as_u64() as i64));
        (ts(self.instrument.activation_ns(), Ts::MIN), ts(self.instrument.expiration_ns(), Ts::MAX))
    }
}

impl Contract for ExchangeFuture {
    fn entity(&self) -> EntityId {
        self.entity
    }

    fn kind(&self) -> &'static str {
        "future"
    }

    fn params(&self) -> u64 {
        let mut h = Hash64::new();
        h.str("future");
        h.str(&self.instrument.id.to_string());
        h.f64(self.multiplier());
        h.f64(self.tick());
        let (a, e) = self.window();
        h.i64(a.0);
        h.i64(e.0);
        h.str(&self.price.0);
        h.str(&self.price.1);
        h.finish()
    }

    fn bind<'a>(&'a self, ws: &'a Workspace) -> Result<Box<dyn Stepper + 'a>> {
        let s = ws.outcome(&self.price.0)?;
        let col = s.column(&self.price.1)?;
        let (activation, expiration) = self.window();
        Ok(Box::new(FutureStep {
            s: s.as_ref(),
            col,
            e: self.entity,
            multiplier: self.multiplier(),
            tick: self.tick(),
            activation,
            expiration,
            last: f64::NAN,
            last_ok: Ts::MIN,
            pending: false,
            expired: false,
        }))
    }

    fn currency(&self) -> Option<EntityId> {
        self.currency
    }

    fn needs_borrow(&self) -> bool {
        false
    }

    fn reads(&self) -> Vec<String> {
        vec![self.price.0.clone()]
    }

    fn supports(&self) -> Vec<&'static str> {
        vec!["variation margin", "activation", "expiration", "tick", "nautilus instrument"]
    }
}

struct FutureStep<'a> {
    s: &'a Series,
    col: usize,
    e: EntityId,
    multiplier: f64,
    tick: f64,
    activation: Ts,
    expiration: Ts,
    last: f64,
    last_ok: Ts,
    pending: bool,
    expired: bool,
}

impl FutureStep<'_> {
    /// The settlement price at `t`, on the tick, under the missing-data policy.
    fn price(&mut self, t: Ts, ctx: &StepCtx<'_>) -> (f64, ValuationState, Ts) {
        let hit = self.s.exact(self.col, self.e, t, Ts::MAX);
        let (p, state) = ctx.resolve(hit, self.last, self.last_ok, t);
        if state == ValuationState::Valid {
            self.last_ok = t;
        }
        let p = if self.tick > 0.0 && p.is_finite() { (p / self.tick).round() * self.tick } else { p };
        (p, state, hit.map_or(t, |h| h.known))
    }

    fn quiet(state: ValuationState, known: Ts, value: f64) -> UnitStep {
        UnitStep { state, known, value, ..UnitStep::default() }
    }
}

impl Stepper for FutureStep<'_> {
    fn init(&mut self, t0: Ts, ctx: &StepCtx<'_>) -> UnitStep {
        if t0 < self.activation {
            self.pending = true;
            return Self::quiet(ValuationState::Pending, t0, f64::NAN);
        }
        if t0 >= self.expiration {
            self.expired = true;
            return Self::quiet(ValuationState::Matured, t0, 0.0);
        }
        let (p, state, known) = self.price(t0, ctx);
        self.last = p;
        Self::quiet(state, known, 0.0)
    }

    fn step(&mut self, _from: Ts, to: Ts, ctx: &StepCtx<'_>) -> UnitStep {
        if self.expired {
            return Self::quiet(ValuationState::Matured, to, 0.0);
        }
        if self.pending {
            // Listed in this interval: it starts at its first price, with nothing to settle.
            return if to < self.activation {
                Self::quiet(ValuationState::Pending, to, f64::NAN)
            } else {
                self.pending = false;
                self.init(to, ctx)
            };
        }
        let (p, state, known) = self.price(to, ctx);
        let mut flows = Flows::default();
        let vm = self.multiplier * (p - self.last);
        flows.add_cash(Role::Performance, if vm.is_finite() { vm } else { 0.0 });
        if p.is_finite() {
            self.last = p;
        }
        let terms = Terms::from_parts((0.0, 0.0), (0.0, 0.0), (0.0, 0.0), 0.0, &flows);
        // The last settlement is at expiration: after it, the contract no longer exists.
        let state = if to >= self.expiration {
            self.expired = true;
            ValuationState::Matured
        } else {
            state
        };
        UnitStep { state, known, value: 0.0, flows, terms, face: f64::NAN, deferred: None, explain: None }
    }
}

/// A roll: `contracts` units of the first contract of `chain` whose expiration is more than
/// `days_before` days away, and none of the others. `chain` lists (contract, expiration) in any
/// order. A negative `contracts` holds a short position.
#[derive(Clone, Debug)]
pub struct Roll {
    /// The contracts and their expirations.
    pub chain: Vec<(EntityId, Ts)>,
    /// Units to hold.
    pub contracts: f64,
    /// Days before expiration to roll.
    pub days_before: i64,
}

impl Roll {
    /// The contract to hold at `t`.
    #[must_use]
    pub fn active(&self, t: Ts) -> Option<EntityId> {
        let mut chain = self.chain.clone();
        chain.sort_by_key(|(_, x)| *x);
        chain.into_iter().find(|(_, x)| x.0 - t.0 > self.days_before * Ts::DAY).map(|(e, _)| e)
    }
}

impl DecisionModel for Roll {
    fn name(&self) -> String {
        "roll".into()
    }

    fn params(&self) -> u64 {
        let mut h = Hash64::new();
        h.str("roll");
        for (e, x) in &self.chain {
            h.u64(u64::from(e.0));
            h.i64(x.0);
        }
        h.f64(self.contracts);
        h.i64(self.days_before);
        h.finish()
    }

    fn decide(&self, ctx: &DecisionCtx<'_>) -> Result<Option<Targets>> {
        let Some(active) = self.active(ctx.t) else {
            return Err(Error::Invalid(format!("no contract of the roll's chain is more than {} days from expiry at {}", self.days_before, ctx.t)));
        };
        Ok(Some(Targets::Quantities(self.chain.iter().map(|(e, _)| (*e, if *e == active { self.contracts } else { 0.0 })).collect())))
    }
}
