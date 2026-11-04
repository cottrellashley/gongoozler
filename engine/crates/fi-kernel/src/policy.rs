//! Policies: explicit, named choices that would otherwise be silent assumptions.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::hash::Hash64;

/// What decisions and models see of values that are revised.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum Information {
    /// Every vintage, each from its publication: a read at knowledge time K sees the latest
    /// vintage published by K. The original is seen from its release, a correction from its
    /// release. Point in time.
    #[default]
    AsKnown,
    /// The first release only: corrections are never seen. Point in time.
    FirstRelease,
    /// The latest value, dated at the first publication, as most legacy frameworks read it. Not
    /// point in time, and reported as such.
    LegacyLatest,
}

/// Which vintage defines outcomes: the values contracts step on.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum Outcomes {
    /// As first booked: the first published value, what the desk's books recorded.
    #[default]
    AsBooked,
    /// Restated: the latest (corrected) value, labelled as restated. For reconciliation.
    Restated,
}

/// The two knowledge choices of a run: what decisions see, and which vintage defines outcomes
/// (spec, change 0). The default is point in time with outcomes as booked.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct KnowledgePolicy {
    /// What decisions and models see.
    pub information: Information,
    /// Which vintage defines outcomes.
    pub outcomes: Outcomes,
}

impl KnowledgePolicy {
    /// Decisions see every vintage from its publication; outcomes as booked. The default.
    pub const AS_KNOWN: Self = Self { information: Information::AsKnown, outcomes: Outcomes::AsBooked };
    /// Decisions see first releases only; outcomes as booked.
    pub const FIRST_RELEASE: Self = Self { information: Information::FirstRelease, outcomes: Outcomes::AsBooked };
    /// The legacy convention: revised values everywhere, dated at first publication.
    pub const LEGACY_LATEST: Self = Self { information: Information::LegacyLatest, outcomes: Outcomes::Restated };

    /// Whether decisions are point in time.
    #[must_use]
    pub fn is_point_in_time(self) -> bool {
        !matches!(self.information, Information::LegacyLatest)
    }

    /// Hashes the policy into a cache key.
    pub fn hash_into(&self, h: &mut Hash64) {
        h.u64(self.information as u64);
        h.u64(self.outcomes as u64);
    }
}

/// What a contract does when a value it needs (a mark, a level) is missing.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub enum MissingPolicy {
    /// Stop: the run fails, naming the first gap.
    #[default]
    Fail,
    /// Carry the last value as stale, for at most `max_age` nanoseconds (`None`: no limit).
    /// A stale bond keeps accruing where accrued is observed.
    Carry { max_age: Option<i64> },
    /// Use an explicit default value (legacy parity only: "price 100"). Counted, never silent.
    Default { value: f64 },
}

impl MissingPolicy {
    /// Mix into a cache key.
    pub fn hash_into(&self, h: &mut Hash64) {
        match self {
            Self::Fail => h.u64(1),
            Self::Carry { max_age } => {
                h.u64(2);
                h.i64(max_age.unwrap_or(-1));
            }
            Self::Default { value } => {
                h.u64(3);
                h.f64(*value);
            }
        }
    }
}

/// How holdings evolve between decisions.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum HoldingPolicy {
    /// Quantities stay fixed between trades; weights drift with prices.
    #[default]
    Drift,
    /// Weights reset to their targets every period, at no cost: one combined return per period
    /// (Σ w × r), as return-based legacy frameworks compute it.
    ResetWeights,
}

/// Counts of every value the run did not observe (invariant I8: nothing silent).
#[derive(Debug, Default)]
pub struct Diagnostics {
    /// Values carried forward as stale.
    pub stale: AtomicU64,
    /// Explicit default values used.
    pub defaults: AtomicU64,
    /// Values missing with no policy to cover them.
    pub missing: AtomicU64,
    /// Trades skipped because the valuation was missing.
    pub skipped_trades: AtomicU64,
}

impl Diagnostics {
    /// Add one to `counter`.
    pub fn count(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// A snapshot: (stale, defaults, missing, skipped trades).
    #[must_use]
    pub fn snapshot(&self) -> (u64, u64, u64, u64) {
        (
            self.stale.load(Ordering::Relaxed),
            self.defaults.load(Ordering::Relaxed),
            self.missing.load(Ordering::Relaxed),
            self.skipped_trades.load(Ordering::Relaxed),
        )
    }
}
