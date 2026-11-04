//! The waterfall: the book's distributor, paying the deal's two priorities of payments in order on
//! each payment date, with its losses, shortfalls and notionals, in whole units where the deal rounds.

use super::*;

impl Distributor for Waterfall {
    fn name(&self) -> String {
        format!("waterfall({})", self.deal.name)
    }

    fn start(&self) -> Box<dyn DistributorState> {
        Box::new(State::default())
    }

    fn distribute(&self, d: &mut Distribution<'_>, state: &mut dyn DistributorState) -> Result<()> {
        let s = state
            .state_any_mut()
            .downcast_mut::<State>()
            .ok_or_else(|| Error::Invalid(format!("deal {}: the waterfall's state is not its own", self.deal.name)))?;
        if !s.started {
            self.open(d, s)?;
        }
        if !s.ended {
            let c = d.collected;
            s.period.interest += c.interest;
            s.period.principal += c.principal;
            s.period.par_drop += c.par_drop;
            s.period_loss += c.loss();
            s.cum_loss += c.loss();
            if self.deal.payments.binary_search(&d.t).is_ok() {
                self.pay(d, s)?;
            }
        }
        d.publish("reinvesting", if self.reinvesting(s, d.t) { 1.0 } else { 0.0 });
        d.publish("event_of_default", if s.default_event { 1.0 } else { 0.0 });
        self.record(d, s);
        Ok(())
    }
}

impl Waterfall {
    /// An amount due, in whole units by the deal's rounding mode.
    pub(super) fn due(&self, x: f64) -> f64 {
        self.deal.rounding.map_or(x, |r| r.round(x))
    }

    /// The smallest whole amount not below `x`: what a cure must pay.
    pub(super) fn up(&self, x: f64) -> f64 {
        self.deal.rounding.map_or(x, |r| r.up(x))
    }

    /// Pays up to `x` out of an account: in whole units where the deal rounds, never a fraction
    /// of a unit, which stays in its account.
    pub(super) fn take(&self, d: &mut Distribution<'_>, a: Acct, x: f64) -> f64 {
        let x = self.deal.rounding.map_or(x, |r| r.down(x.min(avail(d, a))));
        match a {
            Acct::Interest => d.pay_interest(x),
            Acct::Principal => d.pay_principal(x),
        }
    }

    /// Moves up to `x` of interest into the principal account, in whole units where the deal
    /// rounds.
    pub(super) fn to_principal(&self, d: &mut Distribution<'_>, x: f64) -> f64 {
        let x = self.deal.rounding.map_or(x, |r| r.down(x.min(d.interest().max(0.0))));
        d.interest_to_principal(x)
    }

    /// Moves up to `x` of principal into the interest account, in whole units where the deal
    /// rounds.
    pub(super) fn to_interest(&self, d: &mut Distribution<'_>, x: f64) -> f64 {
        let x = self.deal.rounding.map_or(x, |r| r.down(x.min(d.principal().max(0.0))));
        d.principal_to_interest(x)
    }

    /// Splits `total` across `claims` pro rata, none paid more than it is owed.
    pub(super) fn split(&self, total: f64, claims: &[f64]) -> Vec<f64> {
        self.apportion(total, claims, true)
    }

    /// Shares `total` out by `weights` (the equity's faces), with no claim to cap a share.
    pub(super) fn share_out(&self, total: f64, weights: &[f64]) -> Vec<f64> {
        self.apportion(total, weights, false)
    }

    /// Pro rata: exactly where the deal does not round; in whole units where it does, the units
    /// that rounding each share down leaves go to the largest remainders, the earlier first among
    /// equals.
    pub(super) fn apportion(&self, total: f64, claims: &[f64], capped: bool) -> Vec<f64> {
        let sum: f64 = claims.iter().sum();
        if sum <= 0.0 || total <= 0.0 {
            return vec![0.0; claims.len()];
        }
        let cap = |x: f64, c: f64| if capped { x.min(c) } else { x };
        let exact: Vec<f64> = claims.iter().map(|c| cap(total * c / sum, *c)).collect();
        let Some(r) = self.deal.rounding else { return exact };
        let mut shares: Vec<f64> = exact.iter().map(|x| r.down(*x)).collect();
        let left = r.down(total) - shares.iter().sum::<f64>();
        let mut units = (left / r.unit).round().max(0.0) as usize;
        let mut order: Vec<usize> = (0..claims.len()).collect();
        order.sort_by(|&a, &b| (exact[b] - shares[b]).total_cmp(&(exact[a] - shares[a])));
        for i in order {
            if units == 0 {
                break;
            }
            if !capped || shares[i] + r.unit <= claims[i] + 1e-9 {
                shares[i] += r.unit;
                units -= 1;
            }
        }
        shares
    }

    pub(super) fn rate(&self, ws: &Workspace, j: usize, t: Ts) -> Result<f64> {
        let tr = &self.deal.tranches[j];
        Ok(match &tr.coupon {
            Coupon::None => 0.0,
            Coupon::Fixed(r) => *r,
            Coupon::Floating { index: (s, c), spread, floor } => {
                let series = ws.series(s)?;
                let col = series.column(c)?;
                let x = series
                    .asof(col, EntityId::NONE, t, t)
                    .map(|h| h.value)
                    .filter(|v| v.is_finite())
                    .ok_or_else(|| Error::Missing(format!("deal {}: no {s}.{c} known at {t} for the coupon of {}", self.deal.name, tr.name)))?;
                floor.map_or(x + spread, |f| (x + spread).max(f))
            }
        })
    }

    pub(super) fn open(&self, d: &Distribution<'_>, s: &mut State) -> Result<()> {
        let deal = &*self.deal;
        let n = deal.tranches.len();
        s.issued = deal.tranches.iter().map(|tr| tr.issued).collect();
        s.ever.clone_from(&s.issued);
        s.balance = deal.tranches.iter().map(|tr| if tr.issued { tr.balance.unwrap_or(tr.face) } else { 0.0 }).collect();
        s.deferred = vec![0.0; n];
        s.deficiency = vec![0.0; n];
        s.written = vec![0.0; n];
        s.due = vec![0.0; n];
        s.successor = vec![None; n];
        for e in &deal.exercises {
            for (o, r) in e.redeem.iter().zip(&e.replacements) {
                s.successor[*o] = Some(*r);
            }
        }
        s.pending = vec![[0.0; 5]; n];
        s.paid = vec![[0.0; 3]; n + deal.fees.len() + 1];
        s.fee_due = vec![0.0; deal.fees.len()];
        s.fee_carried = vec![0.0; deal.fees.len()];
        s.reserve = deal.reserve;
        s.tripped = vec![false; deal.triggers.len()];
        s.observed = vec![Vec::new(); deal.triggers.len()];
        s.start = d.t;
        s.original_par = d.par();
        s.par_start = s.original_par;
        s.reinvest_until = deal.reinvest_until;
        s.rate = vec![0.0; n];
        for j in 0..n {
            if s.issued[j] {
                s.rate[j] = self.rate(d.view.ws, j, d.t)?;
            }
            if let Claim::InterestOnly { of, ratio } = deal.tranches[j].claim
                && s.issued[j]
                && deal.tranches[j].balance.is_none()
            {
                s.balance[j] = ratio * s.reference(of, s.original_par);
            }
        }
        s.started = true;
        Ok(())
    }

    pub(super) fn reinvesting(&self, s: &State, t: Ts) -> bool {
        !s.ended
            && s.reinvest_until.is_some_and(|u| t < u)
            && !self.deal.triggers.iter().zip(&s.tripped).any(|(tr, &x)| x && tr.effect == Effect::EndReinvestment)
    }

    pub(super) fn holds(&self, when: When, s: &State, t: Ts) -> bool {
        match when {
            When::Always => true,
            When::Reinvesting => self.reinvesting(s, t),
            When::Amortising => !self.reinvesting(s, t),
            When::Tripped(i) => s.tripped[i],
            When::NotTripped(i) => !s.tripped[i],
        }
    }

    /// One payment date.
    pub(super) fn pay(&self, d: &mut Distribution<'_>, s: &mut State) -> Result<()> {
        let deal = &*self.deal;
        let t = d.t;
        let n = deal.tranches.len();
        let days = (t.0 - s.start.0) as f64 / Ts::DAY as f64;
        for j in 0..n {
            s.due[j] = 0.0;
            let tr = &deal.tranches[j];
            if !s.issued[j] || tr.claim == Claim::Equity {
                continue;
            }
            let yf = days / tr.basis;
            if let Shortfall::Defer { compound: true } = tr.shortfall {
                s.deferred[j] += self.due(s.deferred[j] * s.rate[j] * yf);
            }
            s.due[j] = self.due(s.balance[j] * s.rate[j] * yf);
        }
        for (f, fee) in deal.fees.iter().enumerate() {
            s.fee_due[f] = self.due(fee.rate * s.par_start * days / 360.0 + fee.fixed) + s.fee_carried[f];
        }
        s.interest_start = d.interest();
        s.fees_paid = 0.0;
        self.allocate_losses(s);
        self.observe(d, s, days)?;
        let exercise = deal.exercises.iter().find(|e| e.t == t);
        if deal.maturity == Some(t) || exercise.is_some_and(|e| e.kind == RightKind::Call) {
            self.wind_up(d, s);
        } else {
            if let Some(e) = exercise {
                self.expenses(d, s, e);
            }
            for step in &deal.interest {
                if self.holds(step.when, s, t) {
                    self.apply(d, s, &step.action, Acct::Interest, days)?;
                }
            }
            for step in &deal.principal {
                if self.holds(step.when, s, t) {
                    self.apply(d, s, &step.action, Acct::Principal, days)?;
                }
            }
            if let Some(e) = exercise {
                self.redeem(d, s, e)?;
            }
            self.shortfalls(d, s);
            self.notionals(d, s);
        }
        for j in 0..n {
            let p = std::mem::take(&mut s.pending[j]);
            if p.iter().any(|x| *x != 0.0) {
                let f = deal.tranches[j].face;
                s.out.flows.push((t, j, p.map(|x| x / f)));
            }
        }
        for r in 0..s.paid.len() {
            let p = std::mem::take(&mut s.paid[r]);
            if p.iter().any(|x| *x != 0.0) {
                s.out.paid.push((t, r, p));
            }
        }
        s.start = t;
        s.par_start = d.par();
        s.period = Collected::default();
        if !s.ended {
            for j in 0..n {
                if s.issued[j] {
                    s.rate[j] = self.rate(d.view.ws, j, t)?;
                }
            }
        }
        Ok(())
    }

    pub(super) fn allocate_losses(&self, s: &mut State) {
        let loss = std::mem::take(&mut s.period_loss);
        match &self.deal.losses {
            Losses::Kept => {}
            Losses::WriteDown { order, write_up } => {
                if loss > TINY {
                    let mut left = loss;
                    for &j in order {
                        let j = s.live(j);
                        if !s.issued[j] || left <= TINY {
                            continue;
                        }
                        let w = left.min(s.balance[j]).max(0.0);
                        s.balance[j] -= w;
                        s.written[j] += w;
                        s.pending[j][WRITEDOWN] += w;
                        left -= w;
                    }
                } else if loss < -TINY && *write_up {
                    let mut left = -loss;
                    for &j in order.iter().rev() {
                        let j = s.live(j);
                        let u = left.min(s.written[j]);
                        s.balance[j] += u;
                        s.written[j] -= u;
                        s.pending[j][WRITEUP] += u;
                        left -= u;
                    }
                }
            }
            Losses::Ledger(order) => {
                if loss > TINY {
                    let mut left = loss;
                    for &j in order {
                        let j = s.live(j);
                        if !s.issued[j] {
                            continue;
                        }
                        let x = left.min((s.balance[j] - s.deficiency[j]).max(0.0));
                        s.deficiency[j] += x;
                        left -= x;
                    }
                } else if loss < -TINY {
                    let mut left = -loss;
                    for &j in order.iter().rev() {
                        let j = s.live(j);
                        let x = left.min(s.deficiency[j]);
                        s.deficiency[j] -= x;
                        left -= x;
                    }
                }
            }
        }
    }

    pub(super) fn pay_down(&self, d: &mut Distribution<'_>, s: &mut State, order: &[usize], acct: Acct, need: f64) -> f64 {
        let mut left = need.max(0.0);
        let mut paid = 0.0;
        for &j in order {
            let j = s.live(j);
            if left <= TINY || !s.issued[j] || self.deal.tranches[j].claim != Claim::Debt {
                continue;
            }
            let x = self.take(d, acct, left.min(s.balance[j]));
            s.balance[j] -= x;
            s.pending[j][PRINCIPAL] += x;
            s.paid[j][1] += x;
            left -= x;
            paid += x;
        }
        paid
    }

    /// Interest still unpaid after both priorities, by each tranche's terms (F02).
    pub(super) fn shortfalls(&self, d: &mut Distribution<'_>, s: &mut State) {
        let deal = &*self.deal;
        for (j, tr) in deal.tranches.iter().enumerate() {
            let u = std::mem::take(&mut s.due[j]);
            if u <= TINY {
                continue;
            }
            match tr.shortfall {
                Shortfall::Default => {
                    s.deferred[j] += u;
                    if !s.default_event {
                        s.default_event = true;
                        d.note(format!("deal {}: interest on {} unpaid at {}: an event of default", deal.name, tr.name, d.t));
                    }
                }
                Shortfall::Defer { .. } => s.deferred[j] += u,
                Shortfall::Capitalise => {
                    s.balance[j] += u;
                    s.pending[j][PIK] += u;
                }
                Shortfall::Extinguish => {}
            }
        }
        for (f, fee) in deal.fees.iter().enumerate() {
            s.fee_carried[f] = if fee.carry { s.fee_due[f] } else { 0.0 };
            s.fee_due[f] = 0.0;
        }
    }

    /// Interest-only notionals follow their references: the fall is written down (F14).
    pub(super) fn notionals(&self, d: &Distribution<'_>, s: &mut State) {
        let par = d.par();
        for (j, tr) in self.deal.tranches.iter().enumerate() {
            if let Claim::InterestOnly { of, ratio } = tr.claim
                && s.issued[j]
            {
                let new = ratio * s.reference(of, par);
                let drop = s.balance[j] - new;
                if drop > 0.0 {
                    s.pending[j][WRITEDOWN] += drop;
                } else {
                    s.pending[j][WRITEUP] -= drop;
                }
                s.balance[j] = new;
            }
        }
    }

    /// The marks, accounts and pool at this time.
    pub(super) fn record(&self, d: &Distribution<'_>, s: &mut State) {
        let deal = &*self.deal;
        let n = deal.tranches.len();
        let nav = d.nav - d.paid();
        let elapsed = (d.t.0 - s.start.0) as f64 / Ts::DAY as f64;
        let accrued: Vec<f64> = (0..n)
            .map(|j| {
                let tr = &deal.tranches[j];
                if !s.issued[j] || tr.claim == Claim::Equity { 0.0 } else { s.balance[j] * s.rate[j] * elapsed / tr.basis }
            })
            .collect();
        let debt: f64 = (0..n).filter(|&j| s.issued[j] && deal.tranches[j].claim == Claim::Debt).map(|j| s.balance[j] + s.deferred[j] + accrued[j]).sum();
        let book = nav + s.reserve - debt;
        let equity_face: f64 = (0..n).filter(|&j| s.issued[j] && deal.tranches[j].claim == Claim::Equity).map(|j| s.balance[j]).sum();
        for (j, tr) in deal.tranches.iter().enumerate() {
            if !s.ever[j] {
                s.out.marks.push([f64::NAN; 6]);
                continue;
            }
            let b = s.balance[j];
            let clean = match tr.claim {
                Claim::Debt => 100.0,
                Claim::InterestOnly { .. } => 0.0,
                Claim::Equity if equity_face > 0.0 => 100.0 * book.max(0.0) / equity_face,
                Claim::Equity => 0.0,
            };
            let acc = if b > TINY { 100.0 * (accrued[j] + s.deferred[j]) / b } else { 0.0 };
            s.out.marks.push([clean, acc, b / tr.face, b, s.deferred[j], s.deficiency[j]]);
        }
        s.out.accounts.push([d.interest(), d.principal(), s.reserve]);
        let reinvesting = if self.reinvesting(s, d.t) { 1.0 } else { 0.0 };
        s.out.collateral.push([d.par(), d.value(), d.collected.interest, d.collected.principal, s.cum_loss, nav, reinvesting]);
    }
}
