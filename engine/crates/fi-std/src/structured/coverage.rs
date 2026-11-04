//! Coverage tests and triggers: each observed on its dates, cured by diversion where the terms say,
//! and what passing or failing them does.

use super::*;

impl Waterfall {
    /// The triggers, at the payment date's observation point (before the waterfall).
    pub(super) fn observe(&self, d: &Distribution<'_>, s: &mut State, days: f64) -> Result<()> {
        let deal = &*self.deal;
        for (i, tr) in deal.triggers.iter().enumerate() {
            let v = match &tr.metric {
                Metric::CumulativeLoss => s.cum_loss / s.original_par,
                Metric::PoolFactor => d.par() / s.original_par,
                Metric::PaymentRate => s.period.principal / s.par_start,
                Metric::ExcessSpread => {
                    let fees: f64 = s.fee_due.iter().zip(&s.fee_carried).map(|(a, c)| a - c).sum();
                    let owed = fees + s.due.iter().sum::<f64>();
                    (s.period.interest - owed) / s.par_start * 360.0 / days
                }
                Metric::Test(k) => {
                    let (num, den) = self.coverage(d, s, &deal.tests[*k], days)?;
                    ratio(num, den)
                }
                Metric::Series(name, col) => {
                    let series = d.view.series(name)?;
                    let c = series.column(col)?;
                    d.view
                        .asof(series, c, EntityId::NONE)
                        .map(|h| h.value)
                        .filter(|x| x.is_finite())
                        .ok_or_else(|| Error::Missing(format!("deal {}: trigger {} has no {name}.{col} known at {}", deal.name, tr.name, d.t)))?
                }
            };
            if v.is_finite() {
                s.observed[i].push(v);
            }
            let w = tr.lookback.max(1);
            let seen = &s.observed[i];
            let tail = &seen[seen.len().saturating_sub(w)..];
            let avg = if tail.is_empty() { f64::NAN } else { tail.iter().sum::<f64>() / tail.len() as f64 };
            if avg.is_finite() {
                let breach = tr.above.is_some_and(|a| avg > a) || tr.below.is_some_and(|b| avg < b);
                s.tripped[i] = if tr.curable { breach } else { s.tripped[i] || breach };
            }
            let threshold = tr.above.or(tr.below).unwrap_or(f64::NAN);
            let ok = if s.tripped[i] { 0.0 } else { 1.0 };
            s.out.tests.push((d.t, deal.tests.len() + i, [avg, threshold, ok, 0.0, avg, ok]));
        }
        Ok(())
    }

    /// A test's numerator and denominator now.
    pub(super) fn coverage(&self, d: &Distribution<'_>, s: &State, test: &Test, days: f64) -> Result<(f64, f64)> {
        let classes = s.outstanding(&test.classes);
        Ok(match test.kind {
            TestKind::Oc => {
                let num = self.adjusted_par(d)? + d.principal().max(0.0);
                (num, classes.iter().map(|&j| s.balance[j] + s.deferred[j]).sum())
            }
            TestKind::Ic => {
                let den = classes.iter().map(|&j| s.balance[j] * s.rate[j] * days / self.deal.tranches[j].basis).sum();
                (s.interest_start - s.fees_paid, den)
            }
        })
    }

    /// The collateral's par after the haircut rules (F01).
    pub(super) fn adjusted_par(&self, d: &Distribution<'_>) -> Result<f64> {
        let deal = &*self.deal;
        let m = d.holdings.len();
        let par: Vec<f64> = (0..m).map(|p| d.par_of(p)).map(|x| if x.is_finite() { x } else { 0.0 }).collect();
        let mut adjusted: f64 = par.iter().sum();
        let mut taken = vec![false; m];
        for h in &deal.haircuts {
            let series = d.view.series(&h.when.0)?;
            let col = series.column(&h.when.1)?;
            let mut hits: Vec<(usize, f64)> = Vec::new();
            for p in 0..m {
                if taken[p] || par[p] <= 0.0 {
                    continue;
                }
                let e = d.records.entities()[d.holdings[p].0];
                let flag = d.view.asof(series, col, e).map_or(0.0, |x| x.value);
                if flag == 0.0 || !flag.is_finite() {
                    continue;
                }
                let mut frac = f64::INFINITY;
                for c in &h.carry {
                    let x = match c {
                        Carry::Market => d.value_of(p) / par[p],
                        Carry::Column { series, column, per } => d.view.value(series, column, e).map_or(f64::NAN, |v| v / per),
                        Carry::Fixed(f) => *f,
                    };
                    if !x.is_finite() {
                        return Err(Error::Missing(format!(
                            "deal {}: haircut {} has no carrying value for {} at {}",
                            deal.name,
                            h.name,
                            d.view.ws.entities.name(e),
                            d.t
                        )));
                    }
                    frac = frac.min(x);
                }
                hits.push((p, frac.min(1.0)));
            }
            let base: f64 = (0..m).filter(|&p| !taken[p]).map(|p| par[p]).sum();
            match h.excess_over {
                None => {
                    for &(p, f) in &hits {
                        adjusted -= par[p] * (1.0 - f);
                    }
                }
                Some(limit) => {
                    let mut excess = hits.iter().map(|(p, _)| par[*p]).sum::<f64>() - limit * base;
                    hits.sort_by(|a, b| a.1.total_cmp(&b.1));
                    for &(p, f) in &hits {
                        if excess <= 0.0 {
                            break;
                        }
                        let part = excess.min(par[p]);
                        adjusted -= part * (1.0 - f);
                        excess -= part;
                    }
                }
            }
            for &(p, _) in &hits {
                taken[p] = true;
            }
        }
        Ok(adjusted)
    }

    pub(super) fn apply(&self, d: &mut Distribution<'_>, s: &mut State, action: &Action, acct: Acct, days: f64) -> Result<()> {
        let deal = &*self.deal;
        let n = deal.tranches.len();
        match action {
            Action::Fee(f) => {
                let x = self.take(d, acct, s.fee_due[*f]);
                s.fee_due[*f] -= x;
                s.fees_paid += x;
                s.paid[n + *f][2] += x;
            }
            Action::Interest { tranches, reserve } => {
                let js = s.outstanding(tranches);
                let claims: Vec<f64> = js.iter().map(|&j| s.deferred[j] + s.due[j]).collect();
                let total: f64 = claims.iter().sum();
                if total <= TINY {
                    return Ok(());
                }
                let mut paid = self.take(d, acct, total);
                if *reserve && paid < total {
                    let draw = (total - paid).min(s.reserve);
                    s.reserve -= draw;
                    paid += draw;
                }
                let shares = self.split(paid, &claims);
                for (&j, &x) in js.iter().zip(&shares) {
                    let def = x.min(s.deferred[j]);
                    s.deferred[j] -= def;
                    s.due[j] = (s.due[j] - (x - def)).max(0.0);
                    s.pending[j][INTEREST] += x;
                    s.paid[j][0] += x;
                }
            }
            Action::Principal { tranches, pro_rata, schedule } => {
                let js = s.outstanding(tranches);
                let claims: Vec<f64> = js
                    .iter()
                    .map(|&j| {
                        let floor = if *schedule { deal.tranches[j].scheduled(d.t).unwrap_or(s.balance[j]) } else { 0.0 };
                        (s.balance[j] - floor).max(0.0)
                    })
                    .collect();
                let total: f64 = claims.iter().sum();
                let amount = avail(d, acct).min(total);
                if amount <= TINY {
                    return Ok(());
                }
                let mut left = amount;
                let wants = if *pro_rata { self.split(amount, &claims) } else { claims.clone() };
                for (&j, &want) in js.iter().zip(&wants) {
                    let x = self.take(d, acct, want.min(left));
                    s.balance[j] -= x;
                    s.pending[j][PRINCIPAL] += x;
                    s.paid[j][1] += x;
                    left -= x;
                }
            }
            Action::Test(i) => self.test(d, s, *i, acct, days)?,
            Action::Reserve => {
                let x = self.take(d, acct, (deal.reserve_target - s.reserve).max(0.0));
                s.reserve += x;
            }
            Action::Deficiency(order) => {
                for &j in order {
                    let j = s.live(j);
                    s.deficiency[j] -= self.to_principal(d, s.deficiency[j]);
                }
            }
            Action::ToPrincipal => {
                self.to_principal(d, f64::INFINITY);
            }
            Action::ToInterest => {
                self.to_interest(d, f64::INFINITY);
            }
            Action::Residual(j) => {
                let j = s.live(*j);
                let x = self.take(d, acct, f64::INFINITY);
                s.pending[j][INTEREST] += x;
                s.paid[j][0] += x;
            }
        }
        Ok(())
    }

    pub(super) fn test(&self, d: &mut Distribution<'_>, s: &mut State, i: usize, acct: Acct, days: f64) -> Result<()> {
        let deal = &*self.deal;
        let test = &deal.tests[i];
        let thr = test.threshold;
        let (num, den) = self.coverage(d, s, test, days)?;
        let before = ratio(num, den);
        let passed = before >= thr * (1.0 - 1e-12);
        let mut cure = 0.0;
        if !passed {
            match (&test.cure, test.kind) {
                (Cure::None, _) => {}
                (Cure::Reinvest, _) => cure = self.to_principal(d, self.up(thr * den - num)),
                (Cure::Paydown(order), TestKind::Oc) => {
                    // Interest paid as principal lowers the debt only: interest was never part of
                    // the numerator. Principal cash paid out lowers both.
                    let need = match acct {
                        Acct::Interest => den - num / thr,
                        Acct::Principal if thr > 1.0 => (thr * den - num) / (thr - 1.0),
                        Acct::Principal => 0.0,
                    };
                    cure = self.pay_down(d, s, order, acct, self.up(need));
                }
                (Cure::Paydown(order), TestKind::Ic) => {
                    let classes = s.outstanding(&test.classes);
                    let mut den = den;
                    for &j in order {
                        let j = s.live(j);
                        let per = s.rate[j] * days / deal.tranches[j].basis;
                        let gap = den - num / thr;
                        if gap <= TINY {
                            break;
                        }
                        if !classes.contains(&j) || per <= 0.0 {
                            continue;
                        }
                        let x = self.pay_down(d, s, &[j], acct, self.up(gap / per));
                        den -= x * per;
                        cure += x;
                    }
                }
            }
        }
        let (num, den) = self.coverage(d, s, test, days)?;
        let after = ratio(num, den);
        let flag = |x: bool| if x { 1.0 } else { 0.0 };
        s.out.tests.push((d.t, i, [before, thr, flag(passed), cure, after, flag(after >= thr * (1.0 - 1e-12))]));
        Ok(())
    }
}
