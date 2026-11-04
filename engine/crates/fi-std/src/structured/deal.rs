//! The deal as a batch model: its terms checked, its collateral book run on the kernel's ledger with
//! the waterfall as its distributor, and the series it writes.

use super::*;

impl Deal {
    /// The series it writes, in order.
    #[must_use]
    pub fn outputs(&self) -> Vec<String> {
        ["flows", "marks", "distributions", "tests", "accounts", "collateral", "claims"].iter().map(|w| format!("{}.{w}", self.name)).collect()
    }

    pub(super) fn fail<T>(&self, m: impl std::fmt::Display) -> Result<T> {
        Err(Error::Invalid(format!("deal {}: {m}", self.name)))
    }

    /// Checks the terms: every index in range, steps that fit their lists, and rights that
    /// allow each exercise (F04: an exercise before the non-call date, or without the right or
    /// its majority, is refused).
    ///
    /// # Errors
    /// The first inconsistency.
    pub fn validate(&self) -> Result<()> {
        let n = self.tranches.len();
        if n == 0 {
            return self.fail("no tranches");
        }
        let tranche = |j: usize| -> Result<()> { if j < n { Ok(()) } else { self.fail(format!("tranche {j} does not exist")) } };
        for tr in &self.tranches {
            if !(tr.face.is_finite() && tr.face > 0.0) {
                return self.fail(format!("{} has face {}", tr.name, tr.face));
            }
            if tr.balance.is_some_and(|b| !b.is_finite() || b < 0.0) || tr.basis.is_nan() || tr.basis <= 0.0 {
                return self.fail(format!("{} has an invalid balance or day basis", tr.name));
            }
            if let Claim::InterestOnly { of, ratio } = tr.claim {
                if !ratio.is_finite() {
                    return self.fail(format!("{} has notional ratio {ratio}", tr.name));
                }
                if let Notional::Tranche(i) = of {
                    tranche(i)?;
                }
            }
            if tr.schedule.windows(2).any(|w| w[0].0 >= w[1].0) {
                return self.fail(format!("{}'s schedule is not in time order", tr.name));
            }
        }
        for (list, steps) in [("interest", &self.interest), ("principal", &self.principal)] {
            for s in steps {
                match s.when {
                    When::Tripped(i) | When::NotTripped(i) if i >= self.triggers.len() => return self.fail(format!("trigger {i} does not exist")),
                    _ => {}
                }
                match &s.action {
                    Action::Fee(f) if *f >= self.fees.len() => return self.fail(format!("fee {f} does not exist")),
                    Action::Interest { tranches, .. } => tranches.iter().try_for_each(|&j| tranche(j))?,
                    Action::Principal { tranches, .. } => {
                        for &j in tranches {
                            tranche(j)?;
                            if self.tranches[j].claim != Claim::Debt {
                                return self.fail(format!("{} has no principal claim", self.tranches[j].name));
                            }
                        }
                    }
                    Action::Test(i) => {
                        let t = self.tests.get(*i).ok_or_else(|| Error::Invalid(format!("deal {}: test {i} does not exist", self.name)))?;
                        if t.cure == Cure::Reinvest && (t.kind != TestKind::Oc || list != "interest") {
                            return self.fail(format!("test {} cures by reinvesting, which only a par test in the interest list can", t.name));
                        }
                    }
                    Action::Deficiency(js) => {
                        js.iter().try_for_each(|&j| tranche(j))?;
                        if list != "interest" {
                            return self.fail("deficiency ledgers are cured from interest");
                        }
                    }
                    Action::ToPrincipal if list != "interest" => return self.fail("moving interest to principal is an interest step"),
                    Action::ToInterest if list != "principal" => return self.fail("moving principal to interest is a principal step"),
                    Action::Residual(j) => tranche(*j)?,
                    _ => {}
                }
            }
        }
        for t in &self.tests {
            t.classes.iter().try_for_each(|&j| tranche(j))?;
            if let Cure::Paydown(order) = &t.cure {
                order.iter().try_for_each(|&j| tranche(j))?;
            }
            if !(t.threshold.is_finite() && t.threshold > 0.0) {
                return self.fail(format!("test {} has threshold {}", t.name, t.threshold));
            }
        }
        for tr in &self.triggers {
            if tr.above.is_none() && tr.below.is_none() {
                return self.fail(format!("trigger {} has no threshold", tr.name));
            }
            if let Metric::Test(i) = tr.metric
                && i >= self.tests.len()
            {
                return self.fail(format!("trigger {} reads test {i}, which does not exist", tr.name));
            }
        }
        match &self.losses {
            Losses::Kept => {}
            Losses::WriteDown { order, .. } | Losses::Ledger(order) => order.iter().try_for_each(|&j| tranche(j))?,
        }
        for h in &self.haircuts {
            if h.carry.is_empty() {
                return self.fail(format!("haircut {} carries at nothing", h.name));
            }
            if h.excess_over.is_some_and(|x| !(0.0..=1.0).contains(&x)) {
                return self.fail(format!("haircut {}'s excess bucket is not a fraction", h.name));
            }
        }
        if self.payments.windows(2).any(|w| w[0] >= w[1]) {
            return self.fail("payment dates are not strictly increasing");
        }
        if let Some(m) = self.maturity
            && self.payments.binary_search(&m).is_err()
        {
            return self.fail(format!("the maturity {m} is not a payment date"));
        }
        if self.rounding.is_some_and(|r| r.unit.is_nan() || r.unit <= 0.0) {
            return self.fail("the rounding unit is not positive");
        }
        if !(self.cash.is_finite() && self.reserve >= 0.0 && self.reserve_target >= 0.0) {
            return self.fail("the opening cash, reserve or reserve target is invalid");
        }
        let mut replaced = vec![false; n];
        let mut exercises: Vec<&Exercise> = self.exercises.iter().collect();
        exercises.sort_by_key(|e| e.t);
        for e in exercises {
            tranche(e.by)?;
            let by = &self.tranches[e.by].name;
            if self.payments.binary_search(&e.t).is_err() {
                return self.fail(format!("an exercise at {} is not on a payment date", e.t));
            }
            let Some(right) = self.rights.iter().find(|r| r.kind == e.kind && r.holder == e.by) else {
                return self.fail(format!("{by} holds no right to {:?}", e.kind));
            };
            if e.t < right.from {
                return self.fail(format!("{:?} at {} is before the end of the non-call period, {}", e.kind, e.t, right.from));
            }
            if e.share < right.majority {
                return self.fail(format!("{:?} needs {} of {by} to consent, and has {}", e.kind, right.majority, e.share));
            }
            if e.kind == RightKind::Call {
                continue;
            }
            if e.redeem.is_empty() || e.redeem.len() != e.replacements.len() {
                return self.fail("a refinancing redeems classes one for one with their replacements");
            }
            for (&o, &r) in e.redeem.iter().zip(&e.replacements) {
                tranche(o)?;
                tranche(r)?;
                if self.tranches[o].claim != Claim::Debt || self.tranches[r].claim != Claim::Debt {
                    return self.fail("a refinancing redeems and issues debt");
                }
                if self.tranches[r].issued || replaced[r] {
                    return self.fail(format!("{} is outstanding already, so it cannot be issued as a replacement", self.tranches[r].name));
                }
                replaced[r] = true;
            }
        }
        Ok(())
    }

    pub(super) fn reads_all(&self) -> Vec<String> {
        let mut r: BTreeSet<String> = self.collateral.iter().flat_map(|c| c.reads()).collect();
        r.extend(self.reads.iter().cloned());
        for h in &self.haircuts {
            r.insert(h.when.0.clone());
            for c in &h.carry {
                if let Carry::Column { series, .. } = c {
                    r.insert(series.clone());
                }
            }
        }
        for t in &self.triggers {
            if let Metric::Series(s, _) = &t.metric {
                r.insert(s.clone());
            }
        }
        for tr in &self.tranches {
            if let Coupon::Floating { index, .. } = &tr.coupon {
                r.insert(index.0.clone());
            }
        }
        r.into_iter().collect()
    }

    pub(super) fn params(&self) -> u64 {
        let mut h = Hash64::new();
        h.str(&format!(
            "{:?}",
            (
                &self.opening,
                self.cash,
                self.reserve,
                self.reserve_target,
                &self.tranches,
                &self.fees,
                &self.haircuts,
                &self.tests,
                &self.triggers,
                &self.interest,
                &self.principal,
                &self.losses,
            )
        ));
        h.str(&format!(
            "{:?}",
            (&self.payments, self.reinvest_until, self.maturity, &self.rights, &self.exercises, self.missing, self.accounts, self.rounding)
        ));
        for c in &self.collateral {
            h.u64(u64::from(c.entity().0));
            h.str(c.kind());
            h.u64(c.params());
        }
        for m in &self.manager {
            h.str(&m.name);
            h.str(&m.decide.name());
            h.u64(m.decide.params());
            for t in &m.schedule {
                h.i64(t.0);
            }
            h.u64(m.lag as u64);
        }
        h.str(&self.fill.name());
        h.u64(u64::from(self.entity.0));
        h.finish()
    }

    pub(super) fn provenance(&self, what: &str) -> Provenance {
        Provenance {
            knowledge: KnowledgeBasis::Derived,
            producer: Producer::Book { name: self.name.clone() },
            describes: Some(format!("{}.{what}", self.name)),
            intervention: None,
        }
    }

    pub(super) fn written(&self, cal: &Calendar, s: &State) -> Result<Vec<Series>> {
        let times = cal.times().to_vec();
        let n = self.tranches.len();
        let name = |w: &str| format!("{}.{w}", self.name);
        let cols = |names: &[&str], values: Vec<Vec<f64>>| -> Vec<Column> { names.iter().zip(values).map(|(c, v)| Column::new(*c, Unit::Number, v)).collect() };

        let mut fe = Vec::new();
        let mut ft = Vec::new();
        let mut fv = vec![Vec::new(); 5];
        for (t, j, v) in &s.out.flows {
            fe.push(self.tranches[*j].entity);
            ft.push(*t);
            for (c, x) in v.iter().enumerate() {
                fv[c].push(*x);
            }
        }
        let flows = Series::rows(name("flows"), fe, ft.clone(), ft, cols(&["interest", "principal_face", "writedown_face", "writeup_face", "pik_face"], fv))?
            .with_provenance(self.provenance("flows"));

        let mut mv = vec![vec![f64::NAN; times.len() * n]; 6];
        for (r, row) in s.out.marks.iter().enumerate() {
            for (c, x) in row.iter().enumerate() {
                mv[c][r] = *x;
            }
        }
        let entities: Vec<EntityId> = self.tranches.iter().map(|t| t.entity).collect();
        let marks = Series::grid(
            name("marks"),
            entities,
            times.clone(),
            times.clone(),
            cols(&["clean", "accrued", "factor", "balance", "deferred", "deficiency"], mv),
        )?
        .with_provenance(self.provenance("marks"));

        let recipients: Vec<EntityId> = self.tranches.iter().map(|t| t.entity).chain(self.fees.iter().map(|f| f.entity)).chain([self.entity]).collect();
        let mut pe = Vec::new();
        let mut pt = Vec::new();
        let mut pv = vec![Vec::new(); 3];
        for (t, r, v) in &s.out.paid {
            pe.push(recipients[*r]);
            pt.push(*t);
            for (c, x) in v.iter().enumerate() {
                pv[c].push(*x);
            }
        }
        let paid = Series::rows(name("distributions"), pe, pt.clone(), pt, cols(&["interest", "principal", "fee"], pv))?
            .with_provenance(self.provenance("distributions"));

        let mut tt: Vec<Ts> = s.out.tests.iter().map(|(t, _, _)| *t).collect();
        tt.dedup();
        let te: Vec<EntityId> = self.tests.iter().map(|t| t.entity).chain(self.triggers.iter().map(|t| t.entity)).collect();
        let mut tv = vec![vec![f64::NAN; tt.len() * te.len()]; 6];
        for (t, i, v) in &s.out.tests {
            let ti = tt.binary_search(t).expect("a recorded time");
            for (c, x) in v.iter().enumerate() {
                tv[c][ti * te.len() + i] = *x;
            }
        }
        let mut tcols = cols(&["value", "threshold", "passed", "cure", "after", "passed_after"], tv);
        tcols[2].unit = Unit::Flag;
        tcols[5].unit = Unit::Flag;
        let tests = Series::grid(name("tests"), te, tt.clone(), tt, tcols)?.with_provenance(self.provenance("tests"));

        let mut av = vec![vec![f64::NAN; times.len() * 3]];
        for (k, row) in s.out.accounts.iter().enumerate() {
            av[0][k * 3..k * 3 + 3].copy_from_slice(row);
        }
        let accounts = Series::grid(name("accounts"), self.accounts.to_vec(), times.clone(), times.clone(), cols(&["balance"], std::mem::take(&mut av)))?
            .with_provenance(self.provenance("accounts"));

        let mut cv = vec![vec![f64::NAN; times.len()]; 7];
        for (k, row) in s.out.collateral.iter().enumerate() {
            for (c, x) in row.iter().enumerate() {
                cv[c][k] = *x;
            }
        }
        let collateral = Series::grid(
            name("collateral"),
            vec![self.entity],
            times.clone(),
            times,
            cols(&["par", "value", "interest", "principal", "losses", "nav", "reinvesting"], cv),
        )?
        .with_provenance(self.provenance("collateral"));

        let ce: Vec<EntityId> = s.out.claims.iter().map(|(_, new, _, _)| self.tranches[*new].entity).collect();
        let ct: Vec<Ts> = s.out.claims.iter().map(|(t, ..)| *t).collect();
        let replaces = s.out.claims.iter().map(|(_, _, old, _)| self.tranches[*old].entity.as_value()).collect();
        let balance = s.out.claims.iter().map(|(.., b)| *b).collect();
        let claims = Series::rows(
            name("claims"),
            ce,
            ct.clone(),
            ct,
            vec![Column::new("replaces", Unit::Ref, replaces), Column::new("balance", Unit::Number, balance)],
        )?
        .with_provenance(self.provenance("claims"));
        Ok(vec![flows, marks, paid, tests, accounts, collateral, claims])
    }

    /// Runs the deal over `cal`: the collateral book with its waterfall. Returns the waterfall's
    /// final state.
    pub(super) fn run(&self, ws: &Workspace, cal: &Calendar) -> Result<State> {
        let (records, _) = UnitRecords::compute(&self.collateral, ws, cal, self.missing)?;
        let mut ledger = self.book(ws, &records)?;
        while ledger.step()? {}
        ledger
            .distributor_state()
            .and_then(|s| s.state_any().downcast_ref::<State>())
            .cloned()
            .ok_or_else(|| Error::Invalid(format!("deal {}: the waterfall left no state", self.name)))
    }

    /// The collateral book on `records` (the collateral's unit records over the deal's calendar),
    /// with the waterfall as its distributor: what an evaluation steps. Its checkpoints hold the
    /// waterfall's state.
    ///
    /// # Errors
    /// Inconsistent terms (see [`Deal::validate`]), a payment date off the calendar, or the
    /// ledger's own errors.
    pub fn book<'a>(&'a self, ws: &'a Workspace, records: &'a UnitRecords) -> Result<Ledger<'a>> {
        self.validate()?;
        for p in &self.payments {
            if records.calendar().index(*p).is_none() {
                return self.fail(format!("payment date {p} is not on the calendar"));
            }
        }
        let waterfall = Waterfall { deal: Arc::new(self.clone()) };
        let cfg = LedgerConfig {
            initial_cash: self.cash,
            routing: Routing::Collections,
            opening: self.opening.clone(),
            distributor: Some(DistributorRef(Arc::new(waterfall))),
            ..LedgerConfig::default()
        };
        let policies = self
            .manager
            .iter()
            .map(|m| Policy {
                name: m.name.clone(),
                decide: m.decide.as_ref(),
                schedule: m.schedule.clone(),
                scope: None,
                priority: 0,
                lag: m.lag,
                time_in_force: None,
            })
            .collect();
        Ledger::build(ws, records, policies, ExecRef::Fill(self.fill.as_ref()), cfg)
    }
}

impl BatchModel for Deal {
    fn spec(&self) -> ModelSpec {
        let mut spec = ModelSpec::new(&self.name, Shape::Batch);
        spec.writes = self.outputs();
        spec.reads = self.reads_all();
        spec.params = self.params();
        spec.parallel = self.parallel;
        spec.describes = Some(format!("{}.deal", self.name));
        spec
    }

    fn evaluate(&self, ws: &Workspace, cal: &Calendar) -> Result<Vec<Series>> {
        let s = self.run(ws, cal)?;
        self.written(cal, &s)
    }

    fn supports(&self) -> Vec<&'static str> {
        vec![
            "collateral book",
            "manager policies",
            "fees",
            "fixed and floating coupons",
            "deferral",
            "pik",
            "arrears and events of default",
            "oc test",
            "ic test",
            "adjusted collateral haircuts",
            "excess concentration",
            "cure by paydown",
            "cure by reinvestment",
            "reserve",
            "deficiency ledger",
            "write-down and write-up",
            "sequential",
            "pro rata",
            "pac schedule",
            "interest only",
            "principal only",
            "triggers with lookback",
            "curable triggers",
            "early amortization",
            "reinvestment period",
            "call",
            "refinancing",
            "reset",
            "legal final maturity",
        ]
    }

    /// A manager deciding other than as recorded makes the deal a counterfactual: its pool, tests
    /// and distributions are its own, not the real deal's.
    fn intervention(&self, ws: &Workspace) -> Option<Intervention> {
        self.manager.iter().find(|m| m.decide.intervenes(ws)).map(|m| Intervention::Counterfactual(format!("{}'s manager ({})", self.name, m.decide.name())))
    }
}
