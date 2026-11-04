//! Credit default swaps, single-name or index (the brief's R5: credit-event settlement and the
//! remaining index exposure follow the contract).
//!
//! One unit is one unit of original notional of protection bought; a seller holds a negative
//! quantity. The buyer pays the fixed coupon on the notional outstanding, on the premium dates of a
//! declared schedule (quarterly, ACT/360 as usual), and is paid on each reference entity's credit
//! event: one less its recovery (per 100, observed) on that name's weight, less the premium the
//! defaulted part has accrued since the last coupon. The index's outstanding notional (its factor)
//! falls by the name's weight, and the premium after it is on what remains. A name whose event came
//! on or before the first time is out from the start, with nothing paid for it.
//!
//! **Value** is quoted as a price per 100 of current notional from the protection seller's side
//! (100 less the upfront): the buyer's unit is worth the factor times (100 − price) / 100, less the
//! premium accrued since the last coupon. P&L by role: the premium is income (a cost to the buyer),
//! the protection payment against the mark of the notional it settles is `other`, and the rest is
//! price.

use std::sync::Arc;

use fi_kernel::{Contract, EntityId, Error, Flows, Result, Role, Series, StepCtx, Stepper, Terms, Ts, UnitStep, ValuationState, Workspace, hash::Hash64};
use fi_terms::{Date, DayCount, Period, Schedule};

/// A CDS's terms.
#[derive(Clone, Debug, PartialEq)]
pub struct Cds {
    /// The fixed coupon a year, a decimal (0.01, 0.05).
    pub coupon: f64,
    /// The premium periods.
    pub schedule: Schedule,
    /// How the premium accrues.
    pub day_count: DayCount,
    /// The reference entities and their weights (one name of weight one for a single-name CDS).
    pub names: Vec<(String, f64)>,
}

/// A CDS as a contract.
#[derive(Clone, Debug)]
pub struct CdsContract {
    /// The CDS.
    pub entity: EntityId,
    /// Its terms.
    pub terms: Arc<Cds>,
    /// Its quoted price per 100 of current notional, from the seller's side: (series, column).
    pub price: (String, String),
    /// Credit events: a rows series keyed by reference entity, with `default` (1) and `recovery`
    /// (the final price per 100).
    pub events: Option<String>,
}

impl Contract for CdsContract {
    fn entity(&self) -> EntityId {
        self.entity
    }

    fn kind(&self) -> &'static str {
        "cds"
    }

    fn params(&self) -> u64 {
        let mut h = Hash64::new();
        h.str("cds");
        h.str(&format!("{:?}{:?}{:?}", self.terms, self.price, self.events));
        h.finish()
    }

    fn needs_borrow(&self) -> bool {
        false
    }

    fn reads(&self) -> Vec<String> {
        std::iter::once(self.price.0.clone()).chain(self.events.clone()).collect()
    }

    fn marks(&self) -> Vec<String> {
        vec![self.price.0.clone()]
    }

    fn supports(&self) -> Vec<&'static str> {
        vec!["cds", "premium leg", "credit events", "index factor", "auction recovery"]
    }

    fn bind<'a>(&'a self, ws: &'a Workspace) -> Result<Box<dyn Stepper + 'a>> {
        let name = ws.entities.name(self.entity);
        let periods = self.terms.schedule.periods().map_err(|e| Error::Invalid(format!("{name}: {e}")))?;
        let total: f64 = self.terms.names.iter().map(|(_, w)| w).sum();
        if (total - 1.0).abs() > 1e-9 || self.terms.names.iter().any(|(_, w)| *w <= 0.0) {
            return Err(Error::Invalid(format!("{name}: its names' weights add up to {total}, not one")));
        }
        let s = ws.outcome(&self.price.0)?;
        let col = s.column(&self.price.1)?;
        let events = match &self.events {
            Some(ev) => {
                let es = ws.outcome(ev)?;
                let (dc, rc) = (es.column("default")?, es.column("recovery")?);
                let ids = self.terms.names.iter().map(|(n, _)| es.entities().into_iter().find(|e| ws.entities.name(*e) == n.as_str())).collect();
                Some((es.as_ref(), dc, rc, ids))
            }
            None => None,
        };
        Ok(Box::new(CdsStep {
            c: self,
            name,
            periods,
            s: s.as_ref(),
            col,
            events,
            factor: 1.0,
            out: vec![false; self.terms.names.len()],
            paid: 0,
            mark: f64::NAN,
            mark_ok: Ts::MIN,
            accrued: 0.0,
            done: false,
        }))
    }
}

type Events<'a> = (&'a Series, usize, usize, Vec<Option<EntityId>>);

struct CdsStep<'a> {
    c: &'a CdsContract,
    name: &'a str,
    periods: Vec<Period>,
    s: &'a Series,
    col: usize,
    events: Option<Events<'a>>,
    /// Notional outstanding per unit of original notional.
    factor: f64,
    /// Which names have had their credit event.
    out: Vec<bool>,
    /// Premium periods paid.
    paid: usize,
    /// Price per 100, seller's side.
    mark: f64,
    mark_ok: Ts,
    /// Premium accrued per unit, owed by the buyer (positive).
    accrued: f64,
    done: bool,
}

fn date(t: Ts) -> Date {
    Date(t.days() as i32)
}

impl CdsStep<'_> {
    fn fraction(&self, p: &Period, a: Date, b: Date) -> f64 {
        self.c.terms.day_count.fraction(a, b, Some(p.reference), None).unwrap_or(0.0)
    }

    /// Premium accrued on `d` per unit of current notional, owed by the buyer.
    fn accrued_on(&self, d: Date) -> f64 {
        self.periods.iter().find(|p| p.start <= d && d < p.end).map_or(0.0, |p| self.c.terms.coupon * self.fraction(p, p.start, d))
    }

    fn price(&mut self, t: Ts, ctx: &StepCtx<'_>) -> (f64, ValuationState, Ts) {
        let hit = self.s.exact(self.col, self.c.entity, t, Ts::MAX);
        let (p, state) = ctx.resolve(hit, self.mark, self.mark_ok, t);
        if state == ValuationState::Valid {
            self.mark_ok = t;
        }
        (p, state, hit.map_or(t, |h| h.known))
    }

    /// The buyer's clean value per unit at a price.
    fn clean(&self, price: f64) -> f64 {
        self.factor * (100.0 - price) / 100.0
    }
}

impl Stepper for CdsStep<'_> {
    fn init(&mut self, t0: Ts, ctx: &StepCtx<'_>) -> UnitStep {
        let d = date(t0);
        // A name whose credit event came on or before the first time is out from the start: the
        // notional outstanding is what remains, and its protection was settled before.
        let mut known = Ts::MIN;
        if let Some((es, dc, _, ids)) = &self.events {
            for (i, id) in ids.iter().enumerate() {
                let Some(e) = id else { continue };
                let event = es.rows_between(*e, Ts::MIN, t0, Ts::MAX).into_iter().find(|&row| {
                    let v = es.value(*dc, row);
                    v.is_finite() && v != 0.0
                });
                if let Some(row) = event {
                    self.factor -= self.c.terms.names[i].1;
                    self.out[i] = true;
                    known = known.max(es.row_known(row));
                }
            }
        }
        if d >= self.c.terms.schedule.end || self.factor <= 1e-12 {
            self.done = true;
            return UnitStep { state: ValuationState::Matured, known: t0, value: 0.0, ..UnitStep::default() };
        }
        self.paid = self.periods.iter().take_while(|p| p.pay <= d).count();
        let (p, state, k) = self.price(t0, ctx);
        self.mark = p;
        self.accrued = self.accrued_on(d) * self.factor;
        UnitStep { state, known: known.max(k), value: self.clean(p) - self.accrued, face: self.factor, ..UnitStep::default() }
    }

    fn step(&mut self, from: Ts, to: Ts, ctx: &StepCtx<'_>) -> UnitStep {
        if self.done {
            return UnitStep { state: ValuationState::Matured, known: to, value: 0.0, face: 0.0, ..UnitStep::default() };
        }
        let (d0, d1) = (date(from), date(to));
        let (mark0, acc0) = (self.mark, self.accrued);
        let clean0 = self.clean(mark0);
        let (mut premium, mut protection, mut retired, mut known) = (0.0, 0.0, 0.0, Ts::MIN);
        // Credit events in the interval, by name.
        if let Some((es, dc, rc, ids)) = &self.events {
            for (i, id) in ids.iter().enumerate() {
                let Some(e) = id else { continue };
                if self.out[i] {
                    continue;
                }
                for row in es.rows_between(*e, from, to, Ts::MAX) {
                    let (dflt, rec) = (es.value(*dc, row), es.value(*rc, row));
                    if !(dflt.is_finite() && dflt != 0.0) {
                        continue;
                    }
                    if !rec.is_finite() {
                        ctx.fail(format!("{}: a credit event of {} with no recovery", self.name, self.c.terms.names[i].0));
                        continue;
                    }
                    let w = self.c.terms.names[i].1;
                    let de = date(es.row_event(row));
                    // One less the recovery on the name's weight, less what that part accrued.
                    protection += (1.0 - rec / 100.0) * w;
                    premium -= self.accrued_on(de) * w;
                    retired += w * (100.0 - mark0) / 100.0;
                    self.factor -= w;
                    self.out[i] = true;
                    known = known.max(es.row_known(row));
                    break;
                }
            }
        }
        // Premium paid on the dates in the interval, on the notional outstanding then.
        while self.paid < self.periods.len() && self.periods[self.paid].pay <= d1 {
            let p = self.periods[self.paid];
            self.paid += 1;
            if p.pay > d0 {
                premium -= self.c.terms.coupon * self.fraction(&p, p.start, p.end) * self.factor;
            }
        }
        let matured = d1 >= self.c.terms.schedule.end || self.factor <= 1e-12;
        let (price, mut state, k) = if matured { (mark0, ValuationState::Matured, to) } else { self.price(to, ctx) };
        known = known.max(k);
        self.mark = price;
        self.accrued = if matured { 0.0 } else { self.accrued_on(d1) * self.factor };
        let clean1 = if matured { 0.0 } else { self.clean(price) };
        if matured {
            self.done = true;
            state = ValuationState::Matured;
        }
        let mut flows = Flows::default();
        flows.add_cash(Role::Income, premium);
        flows.add_cash(Role::Protection, protection);
        // Accrued premium is the buyer's liability: income's accrual term is its negative. The
        // notional a credit event settles leaves at its old mark: the protection against it is
        // credit (`other`), not principal. At maturity the protection's value expires: price.
        let retired = if retired.is_finite() { retired } else { 0.0 };
        let mut terms = Terms::from_parts((-acc0, -self.accrued), (0.0, 0.0), (clean0, clean1), retired, &flows);
        terms.principal += retired;
        terms.other -= retired;
        UnitStep { state, known, value: clean1 - self.accrued, flows, terms, face: self.factor, deferred: None, explain: None }
    }
}
