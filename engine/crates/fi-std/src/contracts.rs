//! Simple contracts, defined on the kernel's [`Contract`] trait. None needs terms: richer
//! definitions (schedules, rights, credit, structures) come with the debt core in Phase 3.

use fi_kernel::{Contract, EntityId, Flows, Result, Role, Series, StepCtx, Stepper, Terms, Ts, UnitStep, ValuationState, Workspace, hash::Hash64};

fn hash_strs(parts: &[&str]) -> u64 {
    let mut h = Hash64::new();
    for p in parts {
        h.str(p);
    }
    h.finish()
}

fn worse(a: ValuationState, b: ValuationState) -> ValuationState {
    if (b as u8) > (a as u8) { b } else { a }
}

/// A tradable level: an index, or a bond known only by its vendor total return. One unit is
/// worth the level; its P&L is all price (a performance leg).
#[derive(Clone, Debug)]
pub struct TotalReturn {
    /// The contract's entity (the series is keyed by it).
    pub entity: EntityId,
    /// Series holding the level.
    pub series: String,
    /// Level column.
    pub column: String,
}

impl Contract for TotalReturn {
    fn entity(&self) -> EntityId {
        self.entity
    }

    fn kind(&self) -> &'static str {
        "total_return"
    }

    fn supports(&self) -> Vec<&'static str> {
        vec!["level"]
    }

    fn reads(&self) -> Vec<String> {
        vec![self.series.clone()]
    }

    fn params(&self) -> u64 {
        hash_strs(&["total_return", &self.series, &self.column])
    }

    fn bind<'a>(&'a self, ws: &'a Workspace) -> Result<Box<dyn Stepper + 'a>> {
        let s = ws.outcome(&self.series)?;
        let col = s.column(&self.column)?;
        Ok(Box::new(Level { s: s.as_ref(), col, e: self.entity, last: f64::NAN, last_ok: Ts::MIN }))
    }
}

struct Level<'a> {
    s: &'a Series,
    col: usize,
    e: EntityId,
    last: f64,
    last_ok: Ts,
}

impl Stepper for Level<'_> {
    fn init(&mut self, t0: Ts, ctx: &StepCtx<'_>) -> UnitStep {
        let hit = self.s.exact(self.col, self.e, t0, Ts::MAX);
        let (v, state) = ctx.resolve(hit, f64::NAN, Ts::MIN, t0);
        if state == ValuationState::Valid {
            self.last_ok = t0;
        }
        self.last = v;
        UnitStep { state, known: hit.map_or(t0, |h| h.known), value: v, ..UnitStep::default() }
    }

    fn step(&mut self, _from: Ts, to: Ts, ctx: &StepCtx<'_>) -> UnitStep {
        let v0 = self.last;
        let hit = self.s.exact(self.col, self.e, to, Ts::MAX);
        let (v1, state) = ctx.resolve(hit, self.last, self.last_ok, to);
        if state == ValuationState::Valid {
            self.last_ok = to;
        }
        self.last = v1;
        let terms = Terms::from_parts((0.0, 0.0), (0.0, 0.0), (v0, v1), 0.0, &Flows::default());
        UnitStep { state, known: hit.map_or(to, |h| h.known), value: v1, flows: Flows::default(), terms, face: f64::NAN, deferred: None, explain: None }
    }
}

/// A bond priced from supplied marks, with supplied flows: no terms needed.
///
/// One unit is one unit of **original face** (as every legacy framework that tracks face holds
/// it). Marks and accrued interest are per 100 of **current** face. The balance per unit (the
/// factor) starts at 1 and changes only through flows:
///
/// * `interest`: cash per unit of original face (income);
/// * `principal_face`: face retired per unit, settled at `principal_price` per 100 (NaN = 100);
/// * `writedown_face`: face written down per unit, with no cash (a loss at the mark);
/// * `writeup_face`: face written back up per unit, with no cash (a reinstatement);
/// * `pik_face`: face added per unit (PIK interest, paid in kind at par).
///
/// A paydown books `(paydown price − mark) × face` as the principal term: the gain the legacy
/// frameworks lose. A repayment and a write-down can cut the factor by the same amount; only the
/// repayment pays cash.
///
/// A seasoned bond declares a `factor` column in its marks: the balance per unit of original face
/// at the first time (missing there, the bond is unvalued). Without it, the balance starts at 1.
#[derive(Clone, Debug)]
pub struct SuppliedBond {
    /// The bond.
    pub entity: EntityId,
    /// Marks series (grid or rows).
    pub marks: String,
    /// Clean price column, per 100 of current face.
    pub clean: String,
    /// Accrued interest column, per 100 of current face.
    pub accrued: String,
    /// Flows series (rows), if the bond pays anything.
    pub flows: Option<String>,
    /// Marks column holding the factor (current over original face), read at the first time.
    pub factor: Option<String>,
    /// Whether to check that the supplied accrued interest falls back when a coupon pays (review
    /// H1). Income is the change in accrued interest plus the coupon, so a series that never
    /// resets counts every coupon twice, and the P&L identity balances all the same: the check is
    /// the only thing that notices.
    pub reconcile: bool,
}

impl Contract for SuppliedBond {
    fn entity(&self) -> EntityId {
        self.entity
    }

    fn kind(&self) -> &'static str {
        "supplied_bond"
    }

    fn supports(&self) -> Vec<&'static str> {
        let mut s = vec!["supplied marks", "coupon", "amortisation", "paydown price", "writedown", "writeup", "pik", "seasoned factor"];
        if self.reconcile {
            s.push("reconciled accrued");
        }
        s
    }

    fn reads(&self) -> Vec<String> {
        std::iter::once(self.marks.clone()).chain(self.flows.clone()).collect()
    }

    fn marks(&self) -> Vec<String> {
        vec![self.marks.clone()]
    }

    fn params(&self) -> u64 {
        hash_strs(&[
            "supplied_bond",
            &self.marks,
            &self.clean,
            &self.accrued,
            self.flows.as_deref().unwrap_or(""),
            self.factor.as_deref().unwrap_or(""),
            if self.reconcile { "reconciled" } else { "" },
        ])
    }

    fn bind<'a>(&'a self, ws: &'a Workspace) -> Result<Box<dyn Stepper + 'a>> {
        let marks = ws.outcome(&self.marks)?;
        let flows = match &self.flows {
            Some(f) => {
                let s = ws.outcome(f)?;
                Some(FlowCols {
                    s: s.as_ref(),
                    interest: s.column("interest").ok(),
                    principal_face: s.column("principal_face").ok(),
                    principal_price: s.column("principal_price").ok(),
                    writedown_face: s.column("writedown_face").ok(),
                    writeup_face: s.column("writeup_face").ok(),
                    pik_face: s.column("pik_face").ok(),
                })
            }
            None => None,
        };
        Ok(Box::new(Bond {
            marks: marks.as_ref(),
            clean: marks.column(&self.clean)?,
            accrued: marks.column(&self.accrued)?,
            factor: self.factor.as_deref().map(|f| marks.column(f)).transpose()?,
            flows,
            e: self.entity,
            name: ws.entities.name(self.entity),
            balance: 1.0,
            mark: f64::NAN,
            acc: f64::NAN,
            mark_ok: Ts::MIN,
            acc_ok: Ts::MIN,
            reconcile: self.reconcile,
            after_coupon: f64::NAN,
        }))
    }
}

struct FlowCols<'a> {
    s: &'a Series,
    interest: Option<usize>,
    principal_face: Option<usize>,
    principal_price: Option<usize>,
    writedown_face: Option<usize>,
    writeup_face: Option<usize>,
    pik_face: Option<usize>,
}

struct Bond<'a> {
    marks: &'a Series,
    clean: usize,
    accrued: usize,
    factor: Option<usize>,
    flows: Option<FlowCols<'a>>,
    e: EntityId,
    name: &'a str,
    balance: f64,
    mark: f64,
    acc: f64,
    mark_ok: Ts,
    acc_ok: Ts,
    reconcile: bool,
    /// Accrued interest per 100 just after the last coupon (at the first time, before one).
    after_coupon: f64,
}

impl Bond<'_> {
    fn value(&self) -> f64 {
        (self.mark + self.acc) / 100.0 * self.balance
    }

    /// A coupon of `coupon` per 100 paid by `to`: the accrued interest must have fallen back
    /// since the last one. Measured from just after the previous coupon, not across one step, so
    /// steps of a month on quarterly coupons and a coupon gone ex before it pays both reconcile;
    /// only steps as long as a coupon period could hide a reset. Half a coupon of growth is the
    /// tolerance: a series that never resets has grown by a whole one.
    fn check_coupon(&mut self, coupon: f64, to: Ts, ctx: &StepCtx<'_>) {
        if self.reconcile && self.acc.is_finite() && self.after_coupon.is_finite() && self.acc - self.after_coupon > 0.5 * coupon {
            ctx.fail(format!(
                "{}: a coupon of {coupon:.6} per 100 paid by {to}, but the supplied accrued interest rose from {:.6} after the last \
                 coupon to {:.6}: it never fell back, so the coupon would count twice (income is the change in accrued interest plus the \
                 coupon). Supply accrued interest that resets when the coupon pays, or define the bond by its terms",
                self.name, self.after_coupon, self.acc
            ));
        }
        self.after_coupon = self.acc;
    }

    fn read_marks(&mut self, t: Ts, ctx: &StepCtx<'_>) -> (ValuationState, Ts) {
        let mh = self.marks.exact(self.clean, self.e, t, Ts::MAX);
        let ah = self.marks.exact(self.accrued, self.e, t, Ts::MAX);
        let (m, ms) = ctx.resolve(mh, self.mark, self.mark_ok, t);
        let (a, as_) = ctx.resolve(ah, self.acc, self.acc_ok, t);
        if ms == ValuationState::Valid {
            self.mark_ok = t;
        }
        if as_ == ValuationState::Valid {
            self.acc_ok = t;
        }
        self.mark = m;
        self.acc = a;
        let known = mh.map_or(t, |h| h.known).max(ah.map_or(t, |h| h.known));
        (worse(ms, as_), known)
    }
}

impl Stepper for Bond<'_> {
    fn init(&mut self, t0: Ts, ctx: &StepCtx<'_>) -> UnitStep {
        let (mut state, known) = self.read_marks(t0, ctx);
        if let Some(c) = self.factor {
            match self.marks.exact(c, self.e, t0, Ts::MAX) {
                Some(h) if h.value.is_finite() && h.value >= 0.0 => self.balance = h.value,
                _ => {
                    // The units are original face: without the factor, the bond has no value.
                    self.balance = f64::NAN;
                    state = worse(state, ValuationState::Missing);
                }
            }
        }
        self.after_coupon = self.acc;
        UnitStep { state, known, value: self.value(), face: self.balance, ..UnitStep::default() }
    }

    fn step(&mut self, from: Ts, to: Ts, ctx: &StepCtx<'_>) -> UnitStep {
        let (bal0, mark0) = (self.balance, self.mark);
        let clean0 = self.mark / 100.0 * bal0;
        let ai0 = self.acc / 100.0 * bal0;
        let (mut interest, mut pface, mut pcash, mut pik, mut down, mut up) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
        let mut known = Ts::MIN;
        if let Some(f) = &self.flows {
            for row in f.s.rows_between(self.e, from, to, Ts::MAX) {
                let face = c_or_zero(f.principal_face.map(|c| f.s.value(c, row)));
                let price = f.principal_price.map(|c| f.s.value(c, row)).filter(|p| p.is_finite()).unwrap_or(100.0);
                interest += c_or_zero(f.interest.map(|c| f.s.value(c, row)));
                pface += face;
                pcash += face * price / 100.0;
                pik += c_or_zero(f.pik_face.map(|c| f.s.value(c, row)));
                down += c_or_zero(f.writedown_face.map(|c| f.s.value(c, row)));
                up += c_or_zero(f.writeup_face.map(|c| f.s.value(c, row)));
                known = known.max(f.s.row_known(row));
            }
        }
        self.balance = bal0 - pface - down + pik + up;
        if self.balance < -1e-9 * bal0.abs().max(1.0) {
            ctx.fail(format!("{}: the flows to {to} retire {} of face per unit, but only {bal0} is outstanding", self.name, pface + down - pik - up));
        }
        let (mut state, mk) = self.read_marks(to, ctx);
        known = known.max(mk);
        if interest > 0.0 && bal0 > 0.0 {
            self.check_coupon(interest / bal0 * 100.0, to, ctx);
        }
        let clean1 = self.mark / 100.0 * self.balance;
        let ai1 = self.acc / 100.0 * self.balance;
        let mut flows = Flows::default();
        flows.add_cash(Role::Income, interest);
        flows.add_cash(Role::Principal, pcash);
        flows.add_in_kind(Role::Income, pik);
        flows.add_in_kind(Role::Principal, -pik);
        // Face leaving or joining the claim at the old mark: a repayment, a write-down (no cash: a
        // loss), PIK or a write-up (no cash: a gain). The principal term is the cash less this.
        let retired = mark0 / 100.0 * (pface + down - pik - up);
        let terms = Terms::from_parts((ai0, ai1), (0.0, 0.0), (clean0, clean1), retired, &flows);
        if self.balance <= 1e-12 {
            self.balance = 0.0;
            state = ValuationState::Matured;
        }
        UnitStep { state, known, value: self.value(), flows, terms, face: self.balance, deferred: None, explain: None }
    }
}

fn c_or_zero(v: Option<f64>) -> f64 {
    v.filter(|x| x.is_finite()).unwrap_or(0.0)
}

/// Cash: one unit is one unit of currency, earning a market-wide rate in its day count, paid every
/// interval. Without a rate it earns nothing, as most legacy frameworks assume.
#[derive(Clone, Debug)]
pub struct CashAccount {
    /// The cash entity.
    pub entity: EntityId,
    /// Market-wide rate series and column (a fraction per year), read as of each interval's start.
    pub rate: Option<(String, String)>,
    /// How it accrues.
    pub day_count: fi_terms::DayCount,
}

impl Contract for CashAccount {
    fn entity(&self) -> EntityId {
        self.entity
    }

    fn kind(&self) -> &'static str {
        "cash"
    }

    fn supports(&self) -> Vec<&'static str> {
        vec!["deposit interest"]
    }

    fn needs_borrow(&self) -> bool {
        false
    }

    fn reads(&self) -> Vec<String> {
        self.rate.iter().map(|(s, _)| s.clone()).collect()
    }

    fn marks(&self) -> Vec<String> {
        Vec::new()
    }

    fn params(&self) -> u64 {
        let (s, c) = self.rate.as_ref().map_or(("", ""), |(s, c)| (s.as_str(), c.as_str()));
        hash_strs(&["cash", s, c, if self.day_count == fi_terms::DayCount::Act360 { "" } else { self.day_count.name() }])
    }

    fn bind<'a>(&'a self, ws: &'a Workspace) -> Result<Box<dyn Stepper + 'a>> {
        let rate = match &self.rate {
            Some((s, c)) => {
                let s = ws.outcome(s)?;
                Some((s.as_ref(), s.column(c)?))
            }
            None => None,
        };
        if self.day_count.needs_reference() {
            return Err(fi_kernel::Error::Invalid(format!("cash accrues by a day count without coupon periods, not {}", self.day_count)));
        }
        Ok(Box::new(CashStep { rate, day_count: self.day_count }))
    }
}

struct CashStep<'a> {
    rate: Option<(&'a Series, usize)>,
    day_count: fi_terms::DayCount,
}

impl Stepper for CashStep<'_> {
    fn init(&mut self, t0: Ts, _ctx: &StepCtx<'_>) -> UnitStep {
        UnitStep { known: t0, value: 1.0, ..UnitStep::default() }
    }

    fn step(&mut self, from: Ts, to: Ts, _ctx: &StepCtx<'_>) -> UnitStep {
        let hit = self.rate.and_then(|(s, c)| s.asof(c, EntityId::NONE, from, Ts::MAX));
        let r = hit.map_or(0.0, |h| h.value);
        let interest = r * self.day_count.fraction(fi_terms::Date(from.days() as i32), fi_terms::Date(to.days() as i32), None, None).unwrap_or(0.0);
        let mut flows = Flows::default();
        flows.add_cash(Role::Income, interest);
        let terms = Terms::from_parts((0.0, 0.0), (0.0, 0.0), (1.0, 1.0), 0.0, &flows);
        UnitStep {
            state: ValuationState::Valid,
            known: hit.map_or(to, |h| h.known.max(to)),
            value: 1.0,
            flows,
            terms,
            face: f64::NAN,
            deferred: None,
            explain: None,
        }
    }
}

/// A daily-settled future: unfunded, worth zero after each settlement; its P&L is variation
/// margin, (F₁ − F₀) × multiplier, paid as a performance flow.
#[derive(Clone, Debug)]
pub struct DailyFuture {
    /// The contract.
    pub entity: EntityId,
    /// Settlement price series.
    pub series: String,
    /// Settlement price column.
    pub column: String,
    /// Currency per point.
    pub multiplier: f64,
}

impl Contract for DailyFuture {
    fn entity(&self) -> EntityId {
        self.entity
    }

    fn kind(&self) -> &'static str {
        "daily_future"
    }

    fn supports(&self) -> Vec<&'static str> {
        vec!["variation margin"]
    }

    fn needs_borrow(&self) -> bool {
        false
    }

    fn reads(&self) -> Vec<String> {
        vec![self.series.clone()]
    }

    fn params(&self) -> u64 {
        let mut h = Hash64::new();
        h.str("daily_future");
        h.str(&self.series);
        h.str(&self.column);
        h.f64(self.multiplier);
        h.finish()
    }

    fn bind<'a>(&'a self, ws: &'a Workspace) -> Result<Box<dyn Stepper + 'a>> {
        let s = ws.outcome(&self.series)?;
        let col = s.column(&self.column)?;
        Ok(Box::new(Future { level: Level { s: s.as_ref(), col, e: self.entity, last: f64::NAN, last_ok: Ts::MIN }, multiplier: self.multiplier }))
    }
}

struct Future<'a> {
    level: Level<'a>,
    multiplier: f64,
}

impl Stepper for Future<'_> {
    fn init(&mut self, t0: Ts, ctx: &StepCtx<'_>) -> UnitStep {
        let s = self.level.init(t0, ctx);
        UnitStep { value: 0.0, ..s }
    }

    fn step(&mut self, from: Ts, to: Ts, ctx: &StepCtx<'_>) -> UnitStep {
        let s = self.level.step(from, to, ctx);
        let mut flows = Flows::default();
        // The level's price term is F₁ − F₀: settled in cash as variation margin.
        flows.add_cash(Role::Performance, s.terms.price * self.multiplier);
        let terms = Terms::from_parts((0.0, 0.0), (0.0, 0.0), (0.0, 0.0), 0.0, &flows);
        UnitStep { state: s.state, known: s.known, value: 0.0, flows, terms, face: f64::NAN, deferred: None, explain: None }
    }
}
