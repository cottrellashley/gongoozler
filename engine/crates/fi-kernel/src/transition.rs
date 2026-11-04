//! Holding transitions (review H2; spec axiom 5): at an event, every unit of one contract becomes
//! units of others and cash, for whoever holds it.
//!
//! A sovereign exchange turns a defaulted bond into new bonds, cash and a recovery instrument; a
//! conversion turns a convertible into shares; a merger turns one security into another; a claim
//! is extinguished and replaced. A contract's own step cannot do this, since it moves a holding
//! from one contract to others, so transitions are declared with the unit records and the ledger
//! applies them to every book that runs on those records: once, at the event, at the values of the
//! old contract and of what it becomes. The difference is P&L, booked as the declared role's term.
//! The old contract keeps its records: it stays a security that existed, for the universe and for
//! any study of it.

use crate::{
    Error, Result,
    contract::Role,
    entity::{Entities, EntityId},
    time::{Calendar, Ts},
};

/// One holding transition.
#[derive(Clone, Debug, PartialEq)]
pub struct Transition {
    /// When it happens: it applies at the first calendar time at or after this.
    pub at: Ts,
    /// The contract every unit of which transitions.
    pub from: EntityId,
    /// Units of each contract one unit becomes.
    pub into: Vec<(EntityId, f64)>,
    /// Cash one unit becomes, in `from`'s currency.
    pub cash: f64,
    /// The P&L term the difference in value is booked to: `Principal` (an exchange or a
    /// conversion), `Recovery` (a settlement of a defaulted claim, booked as `other`), or any
    /// other role, by the same mapping as a contract's flows.
    pub role: Role,
    /// What it is, for the journal.
    pub note: String,
}

impl Transition {
    /// Checks a set of transitions against the contracts and calendar of the records they belong
    /// to, and returns them in time order.
    ///
    /// # Errors
    /// A contract with no records, a transition at or before the first time (an opening position
    /// is declared as what it is), a contract transitioning twice, a ratio or cash amount that is
    /// not a finite non-negative number (cash may be negative: a payment), a transition into its
    /// own contract, or a chain at one time (one transition's new contract is another's old one at
    /// the same time, so their order would decide the result).
    pub fn validate(mut list: Vec<Self>, has: impl Fn(EntityId) -> bool, cal: &Calendar, names: &Entities) -> Result<Vec<Self>> {
        let first = cal.times().first().copied().unwrap_or(Ts::MIN);
        let step = |t: Ts| cal.times().partition_point(|x| *x < t);
        for (n, tr) in list.iter().enumerate() {
            let name = names.name(tr.from);
            if !has(tr.from) {
                return Err(Error::Unknown(format!("a transition of {name}, which has no records")));
            }
            if tr.at <= first {
                return Err(Error::Invalid(format!("{name} transitions at {}, at or before the first time: declare what it opens as instead", tr.at)));
            }
            if tr.into.is_empty() && tr.cash == 0.0 {
                return Err(Error::Invalid(format!("{name} transitions into nothing")));
            }
            if !tr.cash.is_finite() {
                return Err(Error::Invalid(format!("{name} transitions into {} cash", tr.cash)));
            }
            for (e, r) in &tr.into {
                if !has(*e) {
                    return Err(Error::Unknown(format!("{name} transitions into {}, which has no records", names.name(*e))));
                }
                if *e == tr.from {
                    return Err(Error::Invalid(format!("{name} transitions into itself")));
                }
                if !(r.is_finite() && *r >= 0.0) {
                    return Err(Error::Invalid(format!("{name} transitions into {r} units of {}", names.name(*e))));
                }
            }
            if list[..n].iter().any(|o| o.from == tr.from) {
                return Err(Error::Invalid(format!("{name} transitions twice")));
            }
            for o in &list {
                if step(o.at) == step(tr.at) && o.into.iter().any(|(e, _)| *e == tr.from) {
                    return Err(Error::Invalid(format!(
                        "{} transitions into {name} at the time {name} itself transitions: their order would decide the result",
                        names.name(o.from)
                    )));
                }
            }
        }
        list.sort_by_key(|t| t.at);
        Ok(list)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transitions_are_checked_and_ordered() {
        let mut names = Entities::default();
        let (a, b, c) = (names.intern("bond", "A"), names.intern("bond", "B"), names.intern("bond", "C"));
        let cal = Calendar::new((1..=5).map(Ts::from_days).collect());
        let has = |e: EntityId| [a, b, c].contains(&e);
        let tr = |at: i64, from, into: Vec<(EntityId, f64)>, cash| Transition {
            at: Ts::from_days(at),
            from,
            into,
            cash,
            role: Role::Principal,
            note: String::new(),
        };
        let ok = Transition::validate(vec![tr(4, b, vec![(c, 1.0)], 0.0), tr(2, a, vec![(b, 0.5)], 0.1)], has, &cal, &names).unwrap();
        assert_eq!(ok.iter().map(|t| t.from).collect::<Vec<_>>(), vec![a, b], "in time order");
        for bad in [
            vec![tr(1, a, vec![(b, 1.0)], 0.0)],                                // at the first time
            vec![tr(3, a, vec![], 0.0)],                                        // into nothing
            vec![tr(3, a, vec![(a, 1.0)], 0.0)],                                // into itself
            vec![tr(3, a, vec![(b, -1.0)], 0.0)],                               // a negative ratio
            vec![tr(3, a, vec![(b, 1.0)], 0.0), tr(4, a, vec![(c, 1.0)], 0.0)], // twice
            vec![tr(3, a, vec![(b, 1.0)], 0.0), tr(3, b, vec![(c, 1.0)], 0.0)], // a chain at one time
        ] {
            assert!(Transition::validate(bad, has, &cal, &names).is_err());
        }
        let ghost = EntityId(999);
        assert!(matches!(Transition::validate(vec![tr(3, a, vec![(ghost, 1.0)], 0.0)], has, &cal, &names), Err(Error::Unknown(_))));
    }
}
