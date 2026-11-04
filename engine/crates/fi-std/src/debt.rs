//! A debt instrument defined by its terms (review C1): one contract for bullets, amortisers,
//! sinkers, floaters, linkers, zeros, callables and soft bullets, stepping one unit of original
//! face through the calendar.
//!
//! * **Coupons** come from the terms: the schedule, the day count, the fixings. A coupon is
//!   earned by whoever holds the unit when it goes ex (or, with no ex period, at the end of its
//!   period), and its cash arrives on the payment date, later if need be (a deferred flow).
//! * **Accrued interest** comes from the terms too, so it can never disagree with the coupon
//!   (review H1); after a coupon goes ex it is negative.
//! * **Principal** is repaid on the amortisation schedule and at maturity; observed lifecycle
//!   events change it: a call or put at its price, a default (accrual stops, accrued interest is
//!   written off, the unit trades flat), a recovery, a write-down or write-up, a reported pool
//!   factor, and an extension of a soft bullet.
//! * **Value** is the clean price per 100 of current face plus accrued interest. The clean price is
//!   observed, or comes from an observed yield through the terms, or from a curve and a spread.
//!
//! Everything is read from the outcome view of its series: what happened, as the run's outcome
//! policy defines it. Decisions see only what was known; that is the ledger's business.

use std::cell::Cell;
use std::sync::Arc;

use fi_kernel::{
    Contract, Deferred, EntityId, Error, Flows, Result, Role, Series, StepCtx, Stepper, Terms, Ts, UnitStep, ValuationState, Workspace, hash::Hash64,
};
use fi_terms::analytics::{self, YieldConvention};
use fi_terms::{Coupon, Curve, Date, Debt, Fixings, Interpolation, Period};

/// Where a debt contract's clean price comes from.
#[derive(Clone, Debug, PartialEq)]
pub enum Valuation {
    /// Observed clean prices per 100 of current face: (series, column).
    Clean { series: String, column: String },
    /// Observed yields (decimals), turned into prices through the terms.
    Yield { series: String, column: String, convention: YieldConvention },
    /// A curve observed as zero rates at tenors (a grid series whose entity is the curve and
    /// whose columns are the points), plus a spread over it.
    Curve { series: String, curve: String, points: Vec<(String, f64)>, compounding: Compounding, interpolation: Interpolation, spread: Spread },
}

/// How a curve's observed zero rates compound.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum Compounding {
    /// Continuously.
    #[default]
    Continuous,
    /// Annually.
    Annual,
}

/// The spread over a curve.
#[derive(Clone, Debug, PartialEq)]
pub enum Spread {
    /// None.
    None,
    /// A constant, continuously compounded, as a decimal.
    Constant(f64),
    /// Observed per instrument (decimals): (series, column).
    Observed { series: String, column: String },
}

/// A debt instrument from its terms.
#[derive(Clone, Debug)]
pub struct DebtContract {
    /// The instrument.
    pub entity: EntityId,
    /// Its terms.
    pub terms: Arc<Debt>,
    /// Where its clean price comes from.
    pub valuation: Valuation,
    /// The series its coupon's index fixes from: (series, column). The series is keyed by an
    /// entity named as the coupon's index.
    pub fixings: Option<(String, String)>,
    /// Its lifecycle events: a rows series keyed by the instrument, with any of the columns
    /// `call` and `put` (price per 100), `default` (1), `recovery` (cash per 100 of face) and
    /// `recovery_delay` (calendar days until it pays: the holder when it is determined is paid
    /// later), `writedown` and `writeup` (face per unit of original face), `factor` (face
    /// outstanding per unit of original face, as reported) and `extend` (1).
    pub events: Option<String>,
}

fn hash_valuation(h: &mut Hash64, v: &Valuation) {
    match v {
        Valuation::Clean { series, column } => {
            h.str("clean");
            h.str(series);
            h.str(column);
        }
        Valuation::Yield { series, column, convention } => {
            h.str("yield");
            h.str(series);
            h.str(column);
            h.str(&format!("{convention:?}"));
        }
        Valuation::Curve { series, curve, points, compounding, interpolation, spread } => {
            h.str("curve");
            h.str(series);
            h.str(curve);
            for (c, t) in points {
                h.str(c);
                h.f64(*t);
            }
            h.str(&format!("{compounding:?}{interpolation:?}{spread:?}"));
        }
    }
}

impl Contract for DebtContract {
    fn entity(&self) -> EntityId {
        self.entity
    }

    fn kind(&self) -> &'static str {
        "debt"
    }

    fn params(&self) -> u64 {
        let mut h = Hash64::new();
        h.str("debt");
        h.str(&format!("{:?}", self.terms));
        hash_valuation(&mut h, &self.valuation);
        h.str(&format!("{:?}{:?}", self.fixings, self.events));
        h.finish()
    }

    fn supports(&self) -> Vec<&'static str> {
        let t = &self.terms;
        let mut s = vec!["terms", "coupon schedule", "day count", "accrued from terms", "business days", "paydown price"];
        if t.schedule.ex_days > 0 {
            s.push("ex-coupon");
        }
        match &t.coupon {
            Coupon::Floating { .. } => s.push("floating"),
            Coupon::Inflation { .. } => s.push("inflation"),
            Coupon::Step { .. } => s.push("step"),
            Coupon::Zero => s.push("zero"),
            Coupon::Fixed { .. } => s.push("coupon"),
        }
        if !matches!(t.amortisation, fi_terms::Amortisation::Bullet) {
            s.push("amortisation");
        }
        if t.rights.iter().any(|r| r.kind == fi_terms::RightKind::Call) {
            s.push("call");
        }
        if t.rights.iter().any(|r| r.kind == fi_terms::RightKind::Put) {
            s.push("put");
        }
        if t.extension.is_some() {
            s.push("extension");
        }
        if self.events.is_some() {
            s.extend(["default", "recovery", "writedown", "writeup", "factor"]);
        }
        s.push(match self.valuation {
            Valuation::Clean { .. } => "supplied marks",
            Valuation::Yield { .. } => "yield pricing",
            Valuation::Curve { .. } => "curve pricing",
        });
        s
    }

    fn reads(&self) -> Vec<String> {
        let mut r = vec![match &self.valuation {
            Valuation::Clean { series, .. } | Valuation::Yield { series, .. } | Valuation::Curve { series, .. } => series.clone(),
        }];
        if let Valuation::Curve { spread: Spread::Observed { series, .. }, .. } = &self.valuation {
            r.push(series.clone());
        }
        r.extend(self.fixings.iter().map(|(s, _)| s.clone()));
        r.extend(self.events.iter().cloned());
        r
    }

    fn marks(&self) -> Vec<String> {
        let mut r = vec![match &self.valuation {
            Valuation::Clean { series, .. } | Valuation::Yield { series, .. } | Valuation::Curve { series, .. } => series.clone(),
        }];
        if let Valuation::Curve { spread: Spread::Observed { series, .. }, .. } = &self.valuation {
            r.push(series.clone());
        }
        r
    }

    fn bind<'a>(&'a self, ws: &'a Workspace) -> Result<Box<dyn Stepper + 'a>> {
        let name = ws.entities.name(self.entity);
        let bad = |e: fi_terms::Error| Error::Invalid(format!("{name}: {e}"));
        self.terms.validate().map_err(bad)?;
        let periods = self.terms.periods().map_err(bad)?;
        let extended = match &self.terms.extension {
            Some(x) => {
                let mut t = (*self.terms).clone();
                t.schedule.start = self.terms.maturity();
                t.schedule.end = x.to;
                t.schedule.first = None;
                t.schedule.last = None;
                t.schedule.dates = None;
                if let Some(c) = &x.coupon {
                    t.coupon = c.clone();
                }
                t.amortisation = fi_terms::Amortisation::Bullet;
                t.rights.clear();
                t.extension = None;
                let p = t.periods().map_err(bad)?;
                Some((t, p))
            }
            None => None,
        };
        let fixings = match (&self.fixings, self.terms.coupon.index()) {
            (Some((s, c)), Some(index)) => {
                let series = ws.outcome(s)?;
                let col = series.column(c)?;
                let e = series
                    .entities()
                    .into_iter()
                    .find(|e| ws.entities.name(*e) == index)
                    .ok_or_else(|| Error::Unknown(format!("{name}: the fixings series {s} has no entity named {index}, the coupon's index")))?;
                Some(SeriesFixings { s: series.as_ref(), col, e, known: Cell::new(Ts::MIN) })
            }
            (None, Some(index)) => return Err(Error::Missing(format!("{name}: its coupon fixes on {index}, and no fixings series is given"))),
            _ => None,
        };
        let price = match &self.valuation {
            Valuation::Clean { series, column } => {
                let s = ws.outcome(series)?;
                PriceSource::Clean { col: s.column(column)?, s: s.as_ref() }
            }
            Valuation::Yield { series, column, convention } => {
                let s = ws.outcome(series)?;
                PriceSource::Yield { col: s.column(column)?, s: s.as_ref(), convention: *convention }
            }
            Valuation::Curve { series, curve, points, compounding, interpolation, spread } => {
                let s = ws.outcome(series)?;
                let e = s
                    .entities()
                    .into_iter()
                    .find(|e| ws.entities.name(*e) == curve.as_str())
                    .ok_or_else(|| Error::Unknown(format!("{name}: no curve {curve} in {series}")))?;
                let cols = points.iter().map(|(c, t)| s.column(c).map(|i| (i, *t))).collect::<Result<Vec<_>>>()?;
                let spread = match spread {
                    Spread::None => SpreadSource::Constant(0.0),
                    Spread::Constant(x) => SpreadSource::Constant(*x),
                    Spread::Observed { series, column } => {
                        let ss = ws.outcome(series)?;
                        SpreadSource::Observed { col: ss.column(column)?, s: ss.as_ref() }
                    }
                };
                PriceSource::Curve { s: s.as_ref(), e, cols, compounding: *compounding, interpolation: *interpolation, spread }
            }
        };
        let events = match &self.events {
            Some(ev) => {
                let s = ws.outcome(ev)?;
                let col = |n: &str| s.column(n).ok();
                Some(EventCols {
                    call: col("call"),
                    put: col("put"),
                    default: col("default"),
                    recovery: col("recovery"),
                    recovery_delay: col("recovery_delay"),
                    writedown: col("writedown"),
                    writeup: col("writeup"),
                    factor: col("factor"),
                    extend: col("extend"),
                    s: s.as_ref(),
                })
            }
            None => None,
        };
        // A soft bullet extends when its events say so: decided before anything is paid at maturity.
        let extend_on = events.as_ref().and_then(|ev| {
            let col = ev.extend?;
            ev.s.rows_between(self.entity, Ts::MIN, Ts::MAX, Ts::MAX)
                .into_iter()
                .find(|&r| ev.s.value(col, r).is_finite() && ev.s.value(col, r) != 0.0)
                .map(|r| date(ev.s.row_event(r)))
        });
        if extend_on.is_some() && extended.is_none() {
            return Err(Error::Invalid(format!("{name}: its events extend it, but its terms have no extension")));
        }
        Ok(Box::new(DebtStepper {
            c: self,
            name,
            periods,
            extended,
            extend_on,
            fixings,
            price,
            events,
            balance: 1.0,
            entitled: 0,
            amortised: 0,
            on_extension: false,
            defaulted: false,
            done: false,
            pending: false,
            mark: f64::NAN,
            mark_ok: Ts::MIN,
            accrued: 0.0,
        }))
    }
}

struct SeriesFixings<'a> {
    s: &'a Series,
    col: usize,
    e: EntityId,
    known: Cell<Ts>,
}

impl Fixings for SeriesFixings<'_> {
    fn fixing(&self, _index: &str, date: Date) -> Option<f64> {
        let h = self.s.exact(self.col, self.e, ts(date), Ts::MAX)?;
        self.known.set(self.known.get().max(h.known));
        Some(h.value)
    }
}

enum SpreadSource<'a> {
    Constant(f64),
    Observed { s: &'a Series, col: usize },
}

enum PriceSource<'a> {
    Clean { s: &'a Series, col: usize },
    Yield { s: &'a Series, col: usize, convention: YieldConvention },
    Curve { s: &'a Series, e: EntityId, cols: Vec<(usize, f64)>, compounding: Compounding, interpolation: Interpolation, spread: SpreadSource<'a> },
}

struct EventCols<'a> {
    s: &'a Series,
    call: Option<usize>,
    put: Option<usize>,
    default: Option<usize>,
    recovery: Option<usize>,
    recovery_delay: Option<usize>,
    writedown: Option<usize>,
    writeup: Option<usize>,
    factor: Option<usize>,
    extend: Option<usize>,
}

/// A time's date (midnight UTC, the day it falls on).
fn date(t: Ts) -> Date {
    Date(t.days() as i32)
}

/// A date's time: midnight UTC.
fn ts(d: Date) -> Ts {
    Ts::from_days(i64::from(d.0))
}

struct DebtStepper<'a> {
    c: &'a DebtContract,
    name: &'a str,
    periods: Vec<Period>,
    extended: Option<(Debt, Vec<Period>)>,
    /// When an observed event extended it, if one did.
    extend_on: Option<Date>,
    fixings: Option<SeriesFixings<'a>>,
    price: PriceSource<'a>,
    events: Option<EventCols<'a>>,
    /// Face outstanding per unit of original face.
    balance: f64,
    /// Periods whose coupon has been earned.
    entitled: usize,
    /// Amortisation entries applied.
    amortised: usize,
    on_extension: bool,
    defaulted: bool,
    done: bool,
    /// Before its issue date: it does not exist yet.
    pending: bool,
    /// Clean price per 100 of current face.
    mark: f64,
    mark_ok: Ts,
    /// Accrued interest per unit of original face.
    accrued: f64,
}

/// What happened to the unit in one step.
#[derive(Default)]
struct Moves {
    income_now: f64,
    principal_now: f64,
    recovery_now: f64,
    deferred: Option<Deferred>,
    /// The part of the deferred cash that is a recovery.
    recovery_later: f64,
    /// Face leaving (positive) or joining (negative) the claim at the old mark.
    retired_face: f64,
    /// The part of it settled by a recovery: its P&L against the mark is credit (`other`), not
    /// principal.
    recovered_face: f64,
    known: Ts,
}

impl DebtStepper<'_> {
    fn terms(&self) -> &Debt {
        match &self.extended {
            Some((t, _)) if self.on_extension => t,
            _ => &self.c.terms,
        }
    }

    fn periods(&self) -> &[Period] {
        match &self.extended {
            Some((_, p)) if self.on_extension => p,
            _ => &self.periods,
        }
    }

    fn fixings(&self) -> &dyn Fixings {
        match &self.fixings {
            Some(f) => f,
            None => &fi_terms::coupon::NoFixings,
        }
    }

    /// Interest per unit of current face from a period's start to `d`, indexed for a linker.
    fn interest_per_face(&self, p: &Period, d: Date, ctx: &StepCtx<'_>) -> f64 {
        let t = self.terms();
        let end = d.min(p.end);
        if end <= p.start {
            return 0.0;
        }
        let fx = self.fixings();
        let rate = t.coupon.rate(p, |a, b| t.day_count.fraction(a, b, Some(p.reference), Some(t.maturity())).unwrap_or(0.0), fx);
        let frac = t.fraction(p.start, end, p);
        let ratio = t.coupon.index_ratio(end, fx);
        match (rate, frac, ratio) {
            (Ok(r), Ok(f), Ok(x)) => r * f * x,
            (Err(e), ..) | (_, Err(e), _) | (.., Err(e)) => {
                ctx.fail(format!("{}: {e}", self.name));
                0.0
            }
        }
    }

    /// Accrued interest on `d` per unit of original face: negative once the coupon has gone ex.
    fn accrued_on(&self, d: Date, ctx: &StepCtx<'_>) -> f64 {
        if self.done || self.defaulted || self.balance <= 0.0 {
            return 0.0;
        }
        let Some(p) = self.periods().iter().find(|p| p.start <= d && d < p.end).copied() else { return 0.0 };
        let to_date = self.interest_per_face(&p, d, ctx);
        let a = if p.ex.is_some_and(|ex| d >= ex) { to_date - self.interest_per_face(&p, p.end, ctx) } else { to_date };
        a * self.balance
    }

    /// The clean price per 100 of current face on `to`, and the valuation state.
    fn clean_on(&mut self, to: Ts, ctx: &StepCtx<'_>) -> (f64, ValuationState, Ts) {
        let d = date(to);
        let observed = |s: &Series, col: usize| s.exact(col, self.c.entity, to, Ts::MAX);
        let (hit, derived) = match &self.price {
            PriceSource::Clean { s, col } => (observed(s, *col), None),
            PriceSource::Yield { s, col, convention } => {
                // A yield that cannot be priced (a fixing missing) is no price at all, never the
                // yield read as one.
                let h = observed(s, *col).and_then(|h| {
                    analytics::price_from_yield(self.terms(), d, h.value, *convention, self.fixings()).ok().map(|p| fi_kernel::Hit {
                        value: p.clean,
                        event: h.event,
                        known: h.known,
                    })
                });
                (h, h.map(|h| h.value))
            }
            PriceSource::Curve { s, e, cols, compounding, interpolation, spread } => {
                let mut times = Vec::with_capacity(cols.len());
                let mut zeros = Vec::with_capacity(cols.len());
                let mut known = Ts::MIN;
                let mut complete = true;
                for &(col, t) in cols {
                    match s.exact(col, *e, to, Ts::MAX) {
                        Some(h) => {
                            known = known.max(h.known);
                            times.push(t);
                            zeros.push(match compounding {
                                Compounding::Continuous => h.value,
                                Compounding::Annual => (1.0 + h.value).ln(),
                            });
                        }
                        None => complete = false,
                    }
                }
                let sp = match spread {
                    SpreadSource::Constant(x) => Some((*x, Ts::MIN)),
                    SpreadSource::Observed { s, col } => s.exact(*col, self.c.entity, to, Ts::MAX).map(|h| (h.value, h.known)),
                };
                match (complete, sp, Curve::new(d, times, zeros, *interpolation)) {
                    (true, Some((spread, sk)), Ok(curve)) => {
                        // A floater's coupons not fixed yet are valued at the curve's forwards;
                        // its cash still comes from observed fixings alone.
                        let projected = fi_terms::curve::Projected::for_debt(self.terms(), self.fixings(), &curve);
                        let fx: &dyn Fixings = match &projected {
                            Some(p) => p,
                            None => self.fixings(),
                        };
                        let p = analytics::price_from_curve(self.terms(), d, &curve, spread, fx).ok().map(|p| p.clean);
                        let k = known.max(sk);
                        (p.map(|v| fi_kernel::Hit { value: v, event: to, known: k }), p)
                    }
                    _ => (None, None),
                }
            }
        };
        // A defaulted unit trades flat: only an observed price values it.
        let hit = match (&self.price, self.defaulted) {
            (PriceSource::Clean { .. }, _) | (_, false) => hit.map(|h| fi_kernel::Hit { value: derived.unwrap_or(h.value), event: h.event, known: h.known }),
            _ => None,
        };
        let (v, state) = ctx.resolve(hit, self.mark, self.mark_ok, to);
        if state == ValuationState::Valid {
            self.mark_ok = to;
        }
        (v, state, hit.map_or(to, |h| h.known))
    }

    /// The contractual flows earned or paid in (`d0`, `d1`], and the observed events, in date
    /// order.
    fn moves(&mut self, d0: Date, d1: Date, ctx: &StepCtx<'_>) -> Moves {
        let mut m = Moves { known: Ts::MIN, ..Moves::default() };
        // Observed events, by date.
        let mut events: Vec<(Date, usize)> = Vec::new();
        if let Some(ev) = &self.events {
            for row in ev.s.rows_between(self.c.entity, ts(d0), ts(d1), Ts::MAX) {
                events.push((date(ev.s.row_event(row)), row));
                m.known = m.known.max(ev.s.row_known(row));
            }
        }
        let mut days: Vec<Date> = events.iter().map(|(d, _)| *d).collect();
        days.push(d1);
        days.sort_unstable();
        days.dedup();
        let mut cursor = d0;
        for day in days {
            self.contractual(cursor, day, &mut m, ctx);
            for &(d, row) in events.iter().filter(|(d, _)| *d == day) {
                self.event(d, row, &mut m, ctx);
            }
            cursor = day;
        }
        m
    }

    /// Coupons earned, amortisation and maturity in (`d0`, `d1`].
    fn contractual(&mut self, d0: Date, d1: Date, m: &mut Moves, ctx: &StepCtx<'_>) {
        if self.done || self.defaulted {
            return;
        }
        // Coupons: earned at the ex date, or the period's end; paid on the payment date.
        while self.entitled < self.periods().len() {
            let p = self.periods()[self.entitled];
            let earned = p.ex.unwrap_or(p.end).min(p.end);
            if earned > d1 {
                break;
            }
            self.entitled += 1;
            if earned <= d0 {
                continue;
            }
            let amount = self.terms().round(self.interest_per_face(&p, p.end, ctx) * self.balance);
            if amount == 0.0 {
                continue;
            }
            if p.pay <= d1 {
                m.income_now += amount;
            } else {
                let d = m.deferred.get_or_insert(Deferred { pay: ts(p.pay), cash: 0.0, income: 0.0 });
                d.pay = d.pay.max(ts(p.pay));
                d.cash += amount;
                d.income += amount;
            }
        }
        // Scheduled principal before maturity.
        loop {
            let t = self.terms();
            let fi_terms::Amortisation::Schedule(sched) = &t.amortisation else { break };
            let Some(&(d, face, price)) = sched.get(self.amortised) else { break };
            let pay = t.schedule.calendar.adjust(d, t.schedule.adjust);
            let maturity = t.maturity();
            if pay > d1 {
                break;
            }
            self.amortised += 1;
            if pay <= d0 || d >= maturity {
                continue;
            }
            let ratio = self.terms().coupon.index_ratio(d, self.fixings()).unwrap_or(1.0);
            let face = face.min(self.balance);
            m.principal_now += self.terms().round(face * price / 100.0 * ratio);
            m.retired_face += face;
            self.balance -= face;
        }
        // Maturity: the rest at the redemption price, unless a soft bullet extends.
        let t = self.terms();
        let (maturity, redemption, floors) = (t.maturity(), t.redemption, t.coupon.floors_principal());
        let pay = t.schedule.calendar.adjust(maturity, t.schedule.adjust);
        if pay > d0 && pay <= d1 && self.balance > 0.0 {
            if !self.on_extension && self.extend_on.is_some_and(|x| x <= pay) {
                // Not repaid: the debt carries on to its extended maturity, on its own schedule.
                self.on_extension = true;
                self.entitled = 0;
                self.amortised = usize::MAX;
                return;
            }
            let mut ratio = self.terms().coupon.index_ratio(maturity, self.fixings()).unwrap_or(1.0);
            if floors {
                ratio = ratio.max(1.0);
            }
            m.principal_now += self.terms().round(self.balance * redemption / 100.0 * ratio);
            m.retired_face += self.balance;
            self.balance = 0.0;
            self.done = true;
        }
    }

    /// One observed event on `d`.
    fn event(&mut self, d: Date, row: usize, m: &mut Moves, ctx: &StepCtx<'_>) {
        let Some(ev) = &self.events else { return };
        let get = |c: Option<usize>| c.map(|c| ev.s.value(c, row)).filter(|v| v.is_finite());
        let (call, put, default, recovery) = (get(ev.call), get(ev.put), get(ev.default), get(ev.recovery));
        let (down, up, factor, extend) = (get(ev.writedown), get(ev.writeup), get(ev.factor), get(ev.extend));
        // An extension is decided at bind (see `extend_on`): nothing to do on its day.
        let _ = extend;
        if let Some(f) = factor {
            if f < 0.0 {
                ctx.fail(format!("{}: a factor of {f} on {d}", self.name));
            } else if f < self.balance {
                // Face repaid at par, as the pool reports.
                m.principal_now += self.balance - f;
                m.retired_face += self.balance - f;
                self.balance = f;
            } else if f > self.balance {
                m.retired_face -= f - self.balance;
                self.balance = f;
            }
        }
        if let Some(x) = down {
            let x = x.min(self.balance);
            m.retired_face += x;
            self.balance -= x;
        }
        if let Some(x) = up {
            m.retired_face -= x;
            self.balance += x;
        }
        for (price, kind) in [(call, "call"), (put, "put")] {
            let Some(price) = price else { continue };
            if self.done {
                ctx.fail(format!("{}: a {kind} on {d}, after it was repaid", self.name));
                continue;
            }
            let right = match kind {
                "call" => fi_terms::RightKind::Call,
                _ => fi_terms::RightKind::Put,
            };
            if self.c.terms.right_on(right, d).is_none() {
                ctx.fail(format!("{}: a {kind} on {d}, which its terms do not allow", self.name));
            }
            // Repaid at the price, with the interest accrued to the day (unless the coupon was
            // already earned).
            m.income_now += self.terms().round(self.accrued_on(d, ctx).max(0.0));
            let ratio = self.terms().coupon.index_ratio(d, self.fixings()).unwrap_or(1.0);
            m.principal_now += self.terms().round(self.balance * price / 100.0 * ratio);
            m.retired_face += self.balance;
            self.balance = 0.0;
            self.done = true;
        }
        if default.is_some_and(|x| x != 0.0) && !self.defaulted && !self.done {
            // Accrual stops; the unit trades flat on its defaulted marks.
            self.defaulted = true;
        }
        if let Some(r) = recovery {
            if !self.defaulted {
                ctx.fail(format!("{}: a recovery on {d} without a default", self.name));
            }
            let cash = self.c.terms.round(self.balance * r / 100.0);
            match get(ev.recovery_delay).filter(|x| *x > 0.0) {
                // Determined now, paid later: the holder now owns it, whoever holds the claim then.
                Some(days) => {
                    let pay = ts(d.plus_days(days.round() as i32));
                    let x = m.deferred.get_or_insert(Deferred { pay, cash: 0.0, income: 0.0 });
                    x.pay = x.pay.max(pay);
                    x.cash += cash;
                    m.recovery_later += cash;
                }
                None => m.recovery_now += cash,
            }
            m.retired_face += self.balance;
            m.recovered_face += self.balance;
            self.balance = 0.0;
            self.done = true;
        }
    }
}

impl DebtStepper<'_> {
    /// Before issue the unit does not exist: no value, no marks read, nothing tradable.
    fn not_issued(&mut self, t: Ts) -> Option<UnitStep> {
        self.pending = date(t) < self.c.terms.issue();
        self.pending.then(|| UnitStep { state: ValuationState::Pending, known: t, value: f64::NAN, face: 1.0, ..UnitStep::default() })
    }
}

impl Stepper for DebtStepper<'_> {
    fn init(&mut self, t0: Ts, ctx: &StepCtx<'_>) -> UnitStep {
        if let Some(u) = self.not_issued(t0) {
            return u;
        }
        let d = date(t0);
        // Skip what was earned or repaid before the first time.
        let _ = self.moves(self.c.terms.issue().plus_days(-1), d, ctx);
        if d >= self.c.terms.maturity() && !self.on_extension {
            self.done = true;
        }
        self.balance = if self.done { 0.0 } else { self.c.terms.balance(d).clamp(0.0, 1.0) };
        let (clean, state, known) = self.clean_on(t0, ctx);
        self.mark = clean;
        self.accrued = self.accrued_on(d, ctx);
        let value = clean / 100.0 * self.balance + self.accrued;
        let state = if self.done { ValuationState::Matured } else { state };
        UnitStep { state, known, value: if self.done { 0.0 } else { value }, face: self.balance, ..UnitStep::default() }
    }

    fn step(&mut self, from: Ts, to: Ts, ctx: &StepCtx<'_>) -> UnitStep {
        if self.pending {
            // Issued in this interval: it starts at its first value, with nothing earned before.
            return match self.not_issued(to) {
                Some(u) => u,
                None => self.init(to, ctx),
            };
        }
        let (d0, d1) = (date(from), date(to));
        let (bal0, mark0, ai0) = (self.balance, self.mark, self.accrued);
        let clean0 = mark0 / 100.0 * bal0;
        let was_defaulted = self.defaulted;
        let m = self.moves(d0, d1, ctx);
        let (mut clean, mut state, known) = if self.done { (mark0, ValuationState::Matured, to) } else { self.clean_on(to, ctx) };
        if self.done {
            clean = mark0;
            state = ValuationState::Matured;
        } else if self.defaulted {
            state = ValuationState::Defaulted;
        }
        self.mark = clean;
        self.accrued = self.accrued_on(d1, ctx);
        let clean1 = if self.done { 0.0 } else { clean / 100.0 * self.balance };
        let ai1 = self.accrued;
        let mut flows = Flows::default();
        flows.add_cash(Role::Income, m.income_now);
        flows.add_cash(Role::Principal, m.principal_now);
        flows.add_cash(Role::Recovery, m.recovery_now);
        let mut pnl = flows;
        if let Some(d) = m.deferred {
            pnl.add_cash(Role::Income, d.income);
            pnl.add_cash(Role::Recovery, m.recovery_later);
            pnl.add_cash(Role::Principal, d.cash - d.income - m.recovery_later);
        }
        // Face leaving the claim at the old mark: a repayment at its price books the difference
        // as principal; a write-down, no cash, a loss at the mark. At default the accrued interest
        // is written off (income).
        let retired = mark0.max(0.0) / 100.0 * m.retired_face;
        let retired = if retired.is_finite() { retired } else { 0.0 };
        let mut terms = Terms::from_parts((ai0, ai1), (0.0, 0.0), (clean0, clean1), retired, &pnl);
        // A recovery settles the claim: its gain or loss against the mark is credit, booked with
        // the recovery (`other`) rather than as principal. The total is unchanged.
        let recovered = mark0.max(0.0) / 100.0 * m.recovered_face;
        if recovered.is_finite() && recovered != 0.0 {
            terms.principal += recovered;
            terms.other -= recovered;
        }
        let _ = was_defaulted;
        let known = known.max(m.known).max(self.fixings.as_ref().map_or(Ts::MIN, |f| f.known.get()));
        UnitStep { state, known, value: clean1 + ai1, flows, terms, face: self.balance, deferred: m.deferred }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fi_kernel::{Calendar, Column, MissingPolicy, Unit, UnitRecords};
    use fi_terms::DayCount;

    fn d(s: &str) -> Date {
        s.parse().unwrap()
    }

    fn weekdays(from: &str, to: &str) -> Calendar {
        Calendar::weekdays(ts(d(from)), ts(d(to)))
    }

    /// A workspace with flat clean marks of `clean` for one bond over `cal`.
    fn workspace(cal: &Calendar, clean: f64) -> (Workspace, EntityId) {
        let mut ws = Workspace::new(fi_kernel::KnowledgePolicy::default());
        let e = ws.entities.intern("bond", "B1");
        let times = cal.times().to_vec();
        let marks = Series::grid("marks", vec![e], times.clone(), times.clone(), vec![Column::new("clean", Unit::Price, vec![clean; times.len()])]).unwrap();
        ws.add_series(marks).unwrap();
        (ws, e)
    }

    fn records(ws: &Workspace, c: DebtContract, cal: &Calendar) -> UnitRecords {
        UnitRecords::compute(&[Arc::new(c)], ws, cal, MissingPolicy::Fail).unwrap().0
    }

    #[test]
    fn a_bond_from_its_terms_pays_its_coupons_on_their_dates_and_its_face_at_maturity() {
        let cal = weekdays("2026-01-02", "2027-01-29");
        let (ws, e) = workspace(&cal, 100.0);
        let terms = Debt::fixed("USD", d("2025-07-15"), d("2027-01-15"), 0.05, 2, DayCount::Thirty360).unwrap();
        let rec = records(
            &ws,
            DebtContract {
                entity: e,
                terms: Arc::new(terms),
                valuation: Valuation::Clean { series: "marks".into(), column: "clean".into() },
                fixings: None,
                events: None,
            },
            &cal,
        );
        let times = cal.times();
        let paid: Vec<(String, f64)> = (1..times.len()).filter(|&k| rec.cash[k] != 0.0).map(|k| (times[k].to_string(), rec.cash[k])).collect();
        assert_eq!(paid.len(), 3, "{paid:?}");
        assert_eq!(paid[0].0, "2026-01-15");
        assert!((paid[0].1 - 0.025).abs() < 1e-15 && (paid[2].1 - 1.025).abs() < 1e-15, "{paid:?}");
        // Accrued interest from the terms: 15 Jan to 2 Feb 2026 is 17 days on 30/360.
        let k = cal.index(ts(d("2026-02-02"))).unwrap();
        assert!((rec.value[k] - (1.0 + 0.05 * 17.0 / 360.0)).abs() < 1e-15, "{}", rec.value[k]);
        // Income over the life is exactly the coupons received after the first time, less what was accrued at it.
        let income: f64 = rec.income.iter().sum();
        let ai0 = 0.05 * (5.0 * 30.0 + 17.0) / 360.0; // 15 Jul 2025 to 2 Jan 2026
        assert!((income - (3.0 * 0.025 - ai0)).abs() < 1e-12, "{income}");
    }

    #[test]
    fn an_ex_coupon_is_earned_by_the_holder_on_the_ex_date_and_paid_later() {
        let cal = weekdays("2026-02-02", "2026-04-30");
        let (ws, e) = workspace(&cal, 100.0);
        let mut terms = Debt::fixed("GBP", d("2025-09-15"), d("2030-03-15"), 0.04, 2, DayCount::ActActIcma).unwrap();
        terms.schedule.ex_days = 7;
        let rec = records(
            &ws,
            DebtContract {
                entity: e,
                terms: Arc::new(terms),
                valuation: Valuation::Clean { series: "marks".into(), column: "clean".into() },
                fixings: None,
                events: None,
            },
            &cal,
        );
        // 15 Mar 2026 is a Sunday: paid Monday 16 March, ex seven business days earlier, Thursday 5 March.
        let k = cal.index(ts(d("2026-03-05"))).unwrap();
        assert!((rec.deferred_cash[k] - 0.02).abs() < 1e-15 && Ts(rec.deferred_at[k]) == ts(d("2026-03-16")), "earned on the ex date");
        assert_eq!(rec.cash.iter().filter(|c| **c != 0.0).count(), 0, "no cash moves with the unit");
        assert!(rec.value[k] < 1.0, "the unit is worth less than par ex coupon: {}", rec.value[k]);
    }

    #[test]
    fn a_call_repays_at_its_price_and_a_default_stops_accrual_until_the_recovery() {
        let cal = weekdays("2026-01-02", "2026-12-31");
        let (mut ws, e) = workspace(&cal, 98.0);
        let mut terms = Debt::fixed("USD", d("2025-01-01"), d("2035-01-01"), 0.06, 2, DayCount::Thirty360).unwrap();
        terms.rights.push(fi_terms::Right {
            kind: fi_terms::RightKind::Call,
            from: d("2026-01-01"),
            to: d("2034-12-31"),
            price: 102.0,
            notice: 30,
            make_whole_bp: None,
        });
        let ev =
            Series::rows("events", vec![e], vec![ts(d("2026-07-01"))], vec![ts(d("2026-06-01"))], vec![Column::new("call", Unit::Price, vec![102.0])]).unwrap();
        ws.add_series(ev).unwrap();
        let c = DebtContract {
            entity: e,
            terms: Arc::new(terms.clone()),
            valuation: Valuation::Clean { series: "marks".into(), column: "clean".into() },
            fixings: None,
            events: Some("events".into()),
        };
        let rec = records(&ws, c, &cal);
        let k = cal.index(ts(d("2026-07-01"))).unwrap();
        // The coupon of 1 July and the principal at 102.
        assert!((rec.cash[k] - (0.03 + 1.02)).abs() < 1e-12, "{}", rec.cash[k]);
        assert!((rec.principal[k] - (1.02 - 0.98)).abs() < 1e-12, "the call's premium over the mark: {}", rec.principal[k]);
        assert_eq!(rec.state[k + 1], ValuationState::Matured as u8);

        // A default on 3 Mar, then a recovery of 40 on 2 Oct.
        let (mut ws, e) = workspace(&cal, 35.0);
        let ev = Series::rows(
            "events",
            vec![e, e],
            vec![ts(d("2026-03-03")), ts(d("2026-10-02"))],
            vec![ts(d("2026-03-03")), ts(d("2026-10-02"))],
            vec![Column::new("default", Unit::Number, vec![1.0, f64::NAN]), Column::new("recovery", Unit::Price, vec![f64::NAN, 40.0])],
        )
        .unwrap();
        ws.add_series(ev).unwrap();
        let c = DebtContract {
            entity: e,
            terms: Arc::new(terms),
            valuation: Valuation::Clean { series: "marks".into(), column: "clean".into() },
            fixings: None,
            events: Some("events".into()),
        };
        let rec = records(&ws, c, &cal);
        let kd = cal.index(ts(d("2026-03-03"))).unwrap();
        assert_eq!(rec.state[kd], ValuationState::Defaulted as u8);
        assert!(rec.income[kd] < 0.0, "accrued interest written off at default");
        let k7 = cal.index(ts(d("2026-07-01"))).unwrap();
        assert_eq!(rec.cash[k7], 0.0, "a defaulted bond pays no coupon");
        let kr = cal.index(ts(d("2026-10-02"))).unwrap();
        assert!((rec.cash[kr] - 0.40).abs() < 1e-12 && (rec.other[kr] - (0.40 - 0.35)).abs() < 1e-12, "recovered at 40 against a mark of 35");
    }

    #[test]
    fn a_floater_fixes_its_coupons_and_an_unknown_fixing_fails_the_run() {
        let cal = weekdays("2026-01-02", "2026-07-31");
        let (mut ws, e) = workspace(&cal, 100.0);
        let ix = ws.entities.intern("index", "EUR3M");
        let fix_days = [d("2026-01-13"), d("2026-04-13"), d("2026-07-13")];
        let fixings = Series::rows(
            "ix",
            vec![ix, ix, ix],
            fix_days.iter().map(|x| ts(*x)).collect(),
            fix_days.iter().map(|x| ts(*x)).collect(),
            vec![Column::new("rate", Unit::Rate, vec![0.025, 0.03, 0.028])],
        )
        .unwrap();
        ws.add_series(fixings).unwrap();
        let mut terms = Debt::fixed("EUR", d("2025-10-15"), d("2027-10-15"), 0.0, 4, DayCount::Act360).unwrap();
        terms.coupon = Coupon::Floating {
            index: "EUR3M".into(),
            spread: 0.01,
            gearing: 1.0,
            cap: None,
            floor: Some(0.0),
            method: fi_terms::Method::InAdvance { lag: 2 },
            calendar: fi_terms::BusinessCalendar::weekends(),
        };
        let c = DebtContract {
            entity: e,
            terms: Arc::new(terms.clone()),
            valuation: Valuation::Clean { series: "marks".into(), column: "clean".into() },
            fixings: Some(("ix".into(), "rate".into())),
            events: None,
        };
        // The period from 15 Oct 2025 needs a fixing on 13 Oct 2025, which the series lacks.
        assert!(UnitRecords::compute(&[Arc::new(c)], &ws, &cal, MissingPolicy::Fail).is_err(), "a fixing the run needs and does not have fails it");
        terms.schedule.start = d("2026-01-15");
        let c = DebtContract {
            entity: e,
            terms: Arc::new(terms),
            valuation: Valuation::Clean { series: "marks".into(), column: "clean".into() },
            fixings: Some(("ix".into(), "rate".into())),
            events: None,
        };
        let rec = records(&ws, c, &cal);
        let k = cal.index(ts(d("2026-04-15"))).unwrap();
        // 15 Jan to 15 Apr 2026 is 90 days at 2.5% + 1%.
        assert!((rec.cash[k] - 0.035 * 90.0 / 360.0).abs() < 1e-15, "{}", rec.cash[k]);
    }

    #[test]
    fn a_yield_values_the_unit_through_its_terms() {
        let cal = weekdays("2026-01-15", "2026-01-30");
        let mut ws = Workspace::new(fi_kernel::KnowledgePolicy::default());
        let e = ws.entities.intern("bond", "B1");
        let times = cal.times().to_vec();
        ws.add_series(Series::grid("y", vec![e], times.clone(), times.clone(), vec![Column::new("ytm", Unit::Rate, vec![0.05; times.len()])]).unwrap())
            .unwrap();
        let terms = Debt::fixed("USD", d("2026-01-15"), d("2036-01-15"), 0.05, 2, DayCount::ActActIcma).unwrap();
        let c = DebtContract {
            entity: e,
            terms: Arc::new(terms),
            valuation: Valuation::Yield { series: "y".into(), column: "ytm".into(), convention: YieldConvention::Street },
            fixings: None,
            events: None,
        };
        let rec = records(&ws, c, &cal);
        assert!((rec.value[0] - 1.0).abs() < 1e-9, "at par on its coupon date at its coupon's yield: {}", rec.value[0]);
    }
}
