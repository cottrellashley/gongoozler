//! Unit records: what one unit of each contract earned in each interval.
//!
//! Computed once per contract, calendar and policy set, in parallel across contracts, and shared by
//! every portfolio, benchmark and sweep variant. Storage is columnar and entity-major: one
//! contiguous block of `T` values per contract per field.

use std::collections::HashMap;
use std::sync::Arc;

use rayon::prelude::*;

use crate::{
    Error, Result,
    contract::{Contract, Role, StepCtx, Terms, ValuationState},
    entity::{Entities, EntityId},
    hash::Hash64,
    policy::{Diagnostics, MissingPolicy},
    progress::Reporter,
    series::{ModelRole, Producer},
    time::{Calendar, Ts},
    transition::Transition,
    workspace::Workspace,
};

/// Unit records for a set of contracts over a calendar.
#[derive(Clone, Debug)]
pub struct UnitRecords {
    calendar: Calendar,
    entities: Vec<EntityId>,
    index: HashMap<EntityId, usize>,
    t: usize,
    /// Unit value at each calendar time.
    pub value: Vec<f64>,
    /// Net cash flow per unit in the interval ending at each time.
    pub cash: Vec<f64>,
    /// Income term.
    pub income: Vec<f64>,
    /// Price term.
    pub price: Vec<f64>,
    /// Principal term.
    pub principal: Vec<f64>,
    /// Other flows term.
    pub other: Vec<f64>,
    /// Financing term (a cost).
    pub financing: Vec<f64>,
    /// When the record was known (ns).
    pub known: Vec<i64>,
    /// Valuation state.
    pub state: Vec<u8>,
    /// Principal outstanding per unit (NaN where the contract has none).
    pub face: Vec<f64>,
    /// The income part of each cash flow per unit (the rest is principal and other flows).
    pub cash_income: Vec<f64>,
    /// Cash per unit entitled in the interval but paid later (a coupon gone ex), and its income part.
    pub deferred_cash: Vec<f64>,
    /// The income part of the deferred cash.
    pub deferred_income: Vec<f64>,
    /// When the deferred cash pays (ns; `i64::MIN` when there is none).
    pub deferred_at: Vec<i64>,
    /// Each contract's currency (`EntityId::NONE`: the book's base currency).
    pub currency: Vec<EntityId>,
    /// Whether shorting each contract needs a borrowed security.
    pub needs_borrow: Vec<bool>,
    /// Holding transitions every book on these records applies, in time order.
    transitions: Vec<Transition>,
}

impl UnitRecords {
    /// Steps every contract through `cal`, in parallel.
    ///
    /// # Errors
    /// An empty calendar, duplicate contract entities, unbindable contracts, or (under
    /// [`MissingPolicy::Fail`]) any missing value, naming the first.
    pub fn compute(contracts: &[Arc<dyn Contract>], ws: &Workspace, cal: &Calendar, missing: MissingPolicy) -> Result<(Self, Diagnostics)> {
        if cal.is_empty() {
            return Err(Error::Invalid("unit records need a non-empty calendar".into()));
        }
        let entities: Vec<EntityId> = contracts.iter().map(|c| c.entity()).collect();
        let mut index = HashMap::with_capacity(entities.len());
        for (i, e) in entities.iter().enumerate() {
            if index.insert(*e, i).is_some() {
                return Err(Error::Invalid(format!("two contracts for entity {}", ws.entities.name(*e))));
            }
        }
        let steppers = contracts.iter().map(|c| c.bind(ws)).collect::<Result<Vec<_>>>()?;
        let currency = contracts.iter().map(|c| c.currency().unwrap_or(EntityId::NONE)).collect();
        let needs_borrow = contracts.iter().map(|c| c.needs_borrow()).collect();
        let t = cal.len();
        let n = contracts.len();
        let mut r = Self {
            calendar: cal.clone(),
            entities,
            index,
            t,
            value: vec![0.0; n * t],
            cash: vec![0.0; n * t],
            income: vec![0.0; n * t],
            price: vec![0.0; n * t],
            principal: vec![0.0; n * t],
            other: vec![0.0; n * t],
            financing: vec![0.0; n * t],
            known: vec![0; n * t],
            state: vec![0; n * t],
            face: vec![f64::NAN; n * t],
            cash_income: vec![0.0; n * t],
            deferred_cash: vec![0.0; n * t],
            deferred_income: vec![0.0; n * t],
            deferred_at: vec![i64::MIN; n * t],
            currency,
            needs_borrow,
            transitions: Vec::new(),
        };
        // A value takes observed marks, a pricing model's, a book's, a stress's or a simulated
        // market's: a signal or a forecast is not a mark (spec, change 6).
        for c in contracts {
            for m in c.marks() {
                if let Ok(s) = ws.series(&m)
                    && let Producer::Model { name, role: ModelRole::Signal } = &s.provenance().producer
                {
                    return Err(Error::Invalid(format!(
                        "{} is valued from {m}, the output of model {name}, a signal: a value takes observed marks or a pricing model's (declare the model's role)",
                        ws.entities.name(c.entity())
                    )));
                }
            }
        }
        let diagnostics = Diagnostics::default();
        let broken = std::sync::Mutex::new(None);
        let times = cal.times();
        let reporter = Reporter::start("fi.records", n as u64, "contracts");
        let done = std::sync::atomic::AtomicU64::new(0);
        (
            r.value.par_chunks_mut(t),
            r.cash.par_chunks_mut(t),
            r.income.par_chunks_mut(t),
            r.price.par_chunks_mut(t),
            r.principal.par_chunks_mut(t),
            r.other.par_chunks_mut(t),
            r.financing.par_chunks_mut(t),
            r.known.par_chunks_mut(t),
            r.state.par_chunks_mut(t),
            (
                r.face.par_chunks_mut(t),
                r.cash_income.par_chunks_mut(t),
                r.deferred_cash.par_chunks_mut(t),
                r.deferred_income.par_chunks_mut(t),
                r.deferred_at.par_chunks_mut(t),
            )
                .into_par_iter(),
            steppers.into_par_iter(),
        )
            .into_par_iter()
            .for_each(|(v, c, inc, pr, prin, oth, fin, kn, st, (fc, ci, dc, di, da), mut stepper)| {
                let ctx = StepCtx { missing, diagnostics: &diagnostics, broken: &broken };
                let s0 = stepper.init(times[0], &ctx);
                v[0] = s0.value;
                kn[0] = s0.known.0;
                st[0] = s0.state as u8;
                fc[0] = s0.face;
                for k in 1..t {
                    let s = stepper.step(times[k - 1], times[k], &ctx);
                    v[k] = s.value;
                    c[k] = s.flows.cash_total();
                    ci[k] = s.flows.cash[Role::Income as usize];
                    fc[k] = s.face;
                    inc[k] = s.terms.income;
                    pr[k] = s.terms.price;
                    prin[k] = s.terms.principal;
                    oth[k] = s.terms.other;
                    fin[k] = s.terms.financing;
                    kn[k] = s.known.0;
                    st[k] = s.state as u8;
                    if let Some(d) = s.deferred {
                        dc[k] = d.cash;
                        di[k] = d.income;
                        da[k] = d.pay.0;
                    }
                }
                reporter.report(done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1, None);
            });
        if let Some(m) = broken.into_inner().ok().flatten() {
            return Err(Error::Invalid(m));
        }
        reporter.report(n as u64, None);
        reporter.finish(true);
        if missing == MissingPolicy::Fail && diagnostics.snapshot().2 > 0 {
            let at = r.state.iter().position(|s| *s == ValuationState::Missing as u8).unwrap_or(0);
            let (i, k) = (at / t, at % t);
            return Err(Error::Missing(format!(
                "{} values missing; first: {} at {} (choose a missing-data policy to carry, default or fail)",
                diagnostics.snapshot().2,
                ws.entities.name(r.entities[i]),
                times[k]
            )));
        }
        Ok((r, diagnostics))
    }

    /// Records built directly from arrays (composites and tests).
    ///
    /// # Errors
    /// Mismatched lengths or duplicate entities.
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        calendar: Calendar,
        entities: Vec<EntityId>,
        value: Vec<f64>,
        cash: Vec<f64>,
        terms: Vec<Terms>,
        known: Vec<i64>,
        state: Vec<u8>,
    ) -> Result<Self> {
        let t = calendar.len();
        let n = entities.len();
        if [value.len(), cash.len(), terms.len(), known.len(), state.len()].iter().any(|l| *l != n * t) {
            return Err(Error::Invalid("record arrays must each hold entities × calendar values".into()));
        }
        let mut index = HashMap::with_capacity(n);
        for (i, e) in entities.iter().enumerate() {
            if index.insert(*e, i).is_some() {
                return Err(Error::Invalid(format!("entity {} appears twice", e.0)));
            }
        }
        Ok(Self {
            calendar,
            entities,
            index,
            t,
            value,
            cash,
            income: terms.iter().map(|x| x.income).collect(),
            price: terms.iter().map(|x| x.price).collect(),
            principal: terms.iter().map(|x| x.principal).collect(),
            other: terms.iter().map(|x| x.other).collect(),
            financing: terms.iter().map(|x| x.financing).collect(),
            known,
            state,
            face: vec![f64::NAN; n * t],
            cash_income: vec![0.0; n * t],
            deferred_cash: vec![0.0; n * t],
            deferred_income: vec![0.0; n * t],
            deferred_at: vec![i64::MIN; n * t],
            currency: vec![EntityId::NONE; n],
            needs_borrow: vec![true; n],
            transitions: Vec::new(),
        })
    }

    /// Appends another set of records on the same calendar (for example composites).
    ///
    /// # Errors
    /// A different calendar, or an entity present in both.
    pub fn extend(&mut self, other: &UnitRecords) -> Result<()> {
        if other.calendar != self.calendar {
            return Err(Error::Invalid("records to merge must share a calendar".into()));
        }
        for e in &other.entities {
            if self.index.contains_key(e) {
                return Err(Error::Invalid(format!("entity {} already has records", e.0)));
            }
        }
        for e in &other.entities {
            self.index.insert(*e, self.entities.len());
            self.entities.push(*e);
        }
        self.value.extend(&other.value);
        self.cash.extend(&other.cash);
        self.income.extend(&other.income);
        self.price.extend(&other.price);
        self.principal.extend(&other.principal);
        self.other.extend(&other.other);
        self.financing.extend(&other.financing);
        self.known.extend(&other.known);
        self.state.extend(&other.state);
        self.face.extend(&other.face);
        self.cash_income.extend(&other.cash_income);
        self.deferred_cash.extend(&other.deferred_cash);
        self.deferred_income.extend(&other.deferred_income);
        self.deferred_at.extend(&other.deferred_at);
        self.currency.extend(&other.currency);
        self.needs_borrow.extend(&other.needs_borrow);
        self.transitions.extend(other.transitions.iter().cloned());
        self.transitions.sort_by_key(|t| t.at);
        Ok(())
    }

    /// Declares holding transitions: every book on these records applies them ([`Transition`]).
    ///
    /// # Errors
    /// As [`Transition::validate`], over these records' contracts and calendar, together with
    /// the transitions already declared.
    pub fn with_transitions(&mut self, transitions: Vec<Transition>, names: &Entities) -> Result<()> {
        let mut all = self.transitions.clone();
        all.extend(transitions);
        let index = &self.index;
        self.transitions = Transition::validate(all, |e| index.contains_key(&e), &self.calendar, names)?;
        Ok(())
    }

    /// The holding transitions, in time order.
    #[must_use]
    pub fn transitions(&self) -> &[Transition] {
        &self.transitions
    }

    /// The calendar.
    #[must_use]
    pub fn calendar(&self) -> &Calendar {
        &self.calendar
    }

    /// The contracts' entities, in row order.
    #[must_use]
    pub fn entities(&self) -> &[EntityId] {
        &self.entities
    }

    /// Row of entity `e`.
    #[must_use]
    pub fn idx(&self, e: EntityId) -> Option<usize> {
        self.index.get(&e).copied()
    }

    /// Flat position of (row `i`, calendar index `k`).
    #[must_use]
    pub fn at(&self, i: usize, k: usize) -> usize {
        i * self.t + k
    }

    /// The terms of (row `i`, interval ending at `k`).
    #[must_use]
    pub fn terms(&self, i: usize, k: usize) -> Terms {
        let a = self.at(i, k);
        Terms { income: self.income[a], price: self.price[a], principal: self.principal[a], other: self.other[a], financing: self.financing[a] }
    }

    /// One unit's total return over the interval ending at `k` (NaN when undefined).
    #[must_use]
    pub fn unit_return(&self, i: usize, k: usize) -> f64 {
        if k == 0 {
            return f64::NAN;
        }
        let v0 = self.value[self.at(i, k - 1)];
        if v0.abs() < f64::EPSILON || !v0.is_finite() {
            return f64::NAN;
        }
        self.terms(i, k).total() / v0
    }

    /// The last `n` unit returns up to `k` that were known by `known_by`.
    #[must_use]
    pub fn returns_known(&self, i: usize, k: usize, n: usize, known_by: Ts) -> Vec<f64> {
        let mut out = Vec::with_capacity(n);
        let mut j = k;
        while j >= 1 && out.len() < n {
            if self.known[self.at(i, j)] <= known_by.0 {
                let r = self.unit_return(i, j);
                if r.is_finite() {
                    out.push(r);
                }
            }
            j -= 1;
        }
        out.reverse();
        out
    }

    /// A content hash over every field (invariant I10).
    #[must_use]
    pub fn version(&self) -> u64 {
        let mut h = Hash64::new();
        h.u64(self.calendar.version());
        for e in &self.entities {
            h.u64(u64::from(e.0));
        }
        for arr in [&self.value, &self.cash, &self.income, &self.price, &self.principal, &self.other, &self.financing] {
            for v in arr.iter() {
                h.f64(*v);
            }
        }
        for k in &self.known {
            h.i64(*k);
        }
        for s in &self.state {
            h.u64(u64::from(*s));
        }
        for c in &self.currency {
            h.u64(u64::from(c.0));
        }
        for tr in &self.transitions {
            h.i64(tr.at.0);
            h.u64(u64::from(tr.from.0));
            for (e, r) in &tr.into {
                h.u64(u64::from(e.0));
                h.f64(*r);
            }
            h.f64(tr.cash);
            h.u64(tr.role as u64);
        }
        h.finish()
    }
}
