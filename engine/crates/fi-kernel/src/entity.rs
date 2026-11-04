//! Entities: anything a series describes, interned to a dense index.
//!
//! As Nautilus interns identifiers (`Symbol(Ustr)`), entities are interned once; everything inside
//! the core uses the dense [`EntityId`]. Names appear only at the edges. Relationships between
//! entities (issuer, membership, collateral) are series whose values are entity ids.

use std::collections::HashMap;

/// A dense entity index.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntityId(pub u32);

impl EntityId {
    /// The entity of a market-wide series (a cash rate, an index level).
    pub const NONE: EntityId = EntityId(u32::MAX);

    /// The id as a series value (relationships store references as numbers).
    #[must_use]
    pub fn as_value(self) -> f64 {
        f64::from(self.0)
    }

    /// The id stored in a series value, if the value is a valid reference.
    #[must_use]
    pub fn from_value(v: f64) -> Option<Self> {
        (v.is_finite() && v >= 0.0 && v.fract() == 0.0 && v < f64::from(u32::MAX)).then_some(EntityId(v as u32))
    }
}

/// The entity registry: kind and name per id.
#[derive(Clone, Debug, Default)]
pub struct Entities {
    names: Vec<String>,
    kinds: Vec<String>,
    index: HashMap<String, EntityId>,
}

fn key(kind: &str, name: &str) -> String {
    format!("{kind}\u{1f}{name}")
}

impl Entities {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The id of (`kind`, `name`), registering it if new.
    pub fn intern(&mut self, kind: &str, name: &str) -> EntityId {
        let k = key(kind, name);
        if let Some(id) = self.index.get(&k) {
            return *id;
        }
        let id = EntityId(u32::try_from(self.names.len()).expect("fewer than 2^32 entities"));
        self.names.push(name.to_owned());
        self.kinds.push(kind.to_owned());
        self.index.insert(k, id);
        id
    }

    /// The id of (`kind`, `name`), if registered.
    #[must_use]
    pub fn get(&self, kind: &str, name: &str) -> Option<EntityId> {
        self.index.get(&key(kind, name)).copied()
    }

    /// The name of `id` (`"*"` for [`EntityId::NONE`]).
    #[must_use]
    pub fn name(&self, id: EntityId) -> &str {
        self.names.get(id.0 as usize).map_or("*", String::as_str)
    }

    /// The kind of `id`.
    #[must_use]
    pub fn kind(&self, id: EntityId) -> &str {
        self.kinds.get(id.0 as usize).map_or("*", String::as_str)
    }

    /// Number of entities.
    #[must_use]
    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// Whether no entity is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interning_is_stable_and_kinded() {
        let mut e = Entities::new();
        let a = e.intern("bond", "XS1");
        let b = e.intern("issuer", "XS1");
        assert_ne!(a, b);
        assert_eq!(e.intern("bond", "XS1"), a);
        assert_eq!(e.name(a), "XS1");
        assert_eq!(e.kind(b), "issuer");
        assert_eq!(EntityId::from_value(a.as_value()), Some(a));
        assert_eq!(EntityId::from_value(f64::NAN), None);
    }
}
