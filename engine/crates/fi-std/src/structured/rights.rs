//! Rights and exercises: a call, a refinancing or a liquidation, its expenses, and the wind-up.

use super::*;

impl Waterfall {
    /// A call or the legal final maturity: sell the collateral at its value, pay by seniority,
    /// release the reserve to the equity, and write off what is unpaid.
    pub(super) fn wind_up(&self, d: &mut Distribution<'_>, s: &mut State) {
        let deal = &*self.deal;
        let n = deal.tranches.len();
        d.liquidate();
        self.to_interest(d, f64::INFINITY);
        for f in 0..deal.fees.len() {
            let x = self.take(d, Acct::Interest, s.fee_due[f]);
            s.fee_due[f] -= x;
            s.paid[n + f][2] += x;
        }
        for j in 0..n {
            if s.issued[j] && deal.tranches[j].claim != Claim::Equity {
                let x = self.take(d, Acct::Interest, s.deferred[j] + s.due[j]);
                s.pending[j][INTEREST] += x;
                s.paid[j][0] += x;
            }
        }
        for j in 0..n {
            if s.issued[j] && deal.tranches[j].claim == Claim::Debt {
                let x = self.take(d, Acct::Interest, s.balance[j]);
                s.balance[j] -= x;
                s.pending[j][PRINCIPAL] += x;
                s.paid[j][1] += x;
            }
        }
        let equity: Vec<usize> = (0..n).filter(|&j| s.issued[j] && deal.tranches[j].claim == Claim::Equity).collect();
        let faces: f64 = equity.iter().map(|&j| s.balance[j]).sum();
        let rest = self.deal.rounding.map_or(d.interest().max(0.0), |r| r.down(d.interest().max(0.0)));
        let reserve = std::mem::take(&mut s.reserve);
        let weights: Vec<f64> = equity.iter().map(|&j| if faces > 0.0 { s.balance[j] } else { 1.0 }).collect();
        let (cash, kept) = (self.share_out(rest, &weights), self.share_out(reserve, &weights));
        for (k, &j) in equity.iter().enumerate() {
            let x = self.take(d, Acct::Interest, cash[k]) + kept[k];
            s.pending[j][INTEREST] += x;
            s.paid[j][0] += x;
        }
        for j in 0..n {
            if s.issued[j] && s.balance[j] > 0.0 {
                s.pending[j][WRITEDOWN] += s.balance[j];
                s.balance[j] = 0.0;
            }
            s.deferred[j] = 0.0;
            s.due[j] = 0.0;
            s.deficiency[j] = 0.0;
        }
        s.ended = true;
    }

    pub(super) fn expenses(&self, d: &mut Distribution<'_>, s: &mut State, e: &Exercise) {
        let from_interest = self.take(d, Acct::Interest, e.expenses);
        let rest = e.expenses - from_interest;
        let slot = self.deal.tranches.len() + self.deal.fees.len();
        s.paid[slot][2] += e.expenses;
        if rest > TINY {
            let j = s.live(e.by);
            s.pending[j][INTEREST] -= rest;
            s.paid[j][0] -= rest;
        }
    }

    /// A refinancing's redemptions: each old claim is paid once, at par plus anything unpaid,
    /// from its replacement's proceeds, and the replacement is issued with its own terms.
    pub(super) fn redeem(&self, d: &Distribution<'_>, s: &mut State, e: &Exercise) -> Result<()> {
        let deal = &*self.deal;
        for (&old, &new) in e.redeem.iter().zip(&e.replacements) {
            if !s.issued[old] {
                return Err(Error::Invalid(format!("deal {}: {} is not outstanding at {}", deal.name, deal.tranches[old].name, d.t)));
            }
            let b = s.balance[old];
            let unpaid = s.deferred[old] + s.due[old];
            s.pending[old][PRINCIPAL] += b;
            s.pending[old][INTEREST] += unpaid;
            s.paid[old][1] += b;
            s.paid[old][0] += unpaid;
            s.balance[old] = 0.0;
            s.deferred[old] = 0.0;
            s.due[old] = 0.0;
            s.issued[old] = false;
            s.issued[new] = true;
            s.ever[new] = true;
            s.balance[new] = b + unpaid;
            s.rate[new] = self.rate(d.view.ws, new, d.t)?;
            s.out.claims.push((d.t, new, old, b + unpaid));
        }
        if e.kind == RightKind::Reset
            && let Some(u) = e.reinvest_until
        {
            s.reinvest_until = Some(u);
        }
        Ok(())
    }
}
