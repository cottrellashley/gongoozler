//! The workspace: entities, series and the model cache.
//!
//! Registered series keep every vintage. Two views read them (spec, change 0):
//!
//! * the **information** view, for models and decisions: as known (every vintage from its
//!   publication), first release only, or the legacy latest;
//! * the **outcome** view, for contracts: as booked (first published) or restated (latest).
//!
//! A series without revisions is the same in every view and is stored once.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use crate::{
    Error, Result,
    entity::Entities,
    model::Shape,
    policy::{Information, KnowledgePolicy, Outcomes},
    series::{Intervention, ModelRole, Producer, Series, Vintage},
    time::Ts,
};

/// What produced a model's outputs in this workspace: its declaration's fingerprint and what
/// the run manifest reports of it (spec, change 7).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ModelRecord {
    /// The model.
    pub name: String,
    /// Its declaration's fingerprint (parameters included).
    pub fingerprint: u64,
    /// The content key its outputs are cached under (its inputs' versions included).
    pub key: u64,
    /// Its role.
    pub role: ModelRole,
    /// Its shape.
    pub shape: Shape,
    /// When it became available.
    pub available_from: Option<Ts>,
    /// Used before it was available, as a labelled retrospective experiment.
    pub retrospective: bool,
    /// Its versions: (available from, fingerprint).
    pub vintages: Vec<(Ts, u64)>,
    /// The series it writes.
    pub writes: Vec<String>,
    /// Why its outputs are not what happened, if they are not.
    pub intervention: Option<Intervention>,
    /// States replaced by observations: (state, observation).
    pub substituted: Vec<(String, String)>,
}

/// One registered series: every vintage, and the two single-vintage resolutions.
#[derive(Clone, Debug)]
struct Registered {
    raw: Arc<Series>,
    first: Arc<Series>,
    latest: Arc<Series>,
}

impl Registered {
    fn new(series: Series) -> Self {
        let raw = Arc::new(series);
        if !raw.has_revisions() {
            return Self { first: raw.clone(), latest: raw.clone(), raw };
        }
        let first = Arc::new(raw.resolve(Vintage::First));
        let latest = Arc::new(raw.resolve(Vintage::Latest));
        Self { raw, first, latest }
    }
}

/// Everything a run reads: the entity registry, the series, and cached model outputs.
#[derive(Clone, Debug, Default)]
pub struct Workspace {
    /// The entity registry.
    pub entities: Entities,
    knowledge: KnowledgePolicy,
    registered: HashMap<String, Registered>,
    derived: HashMap<String, Arc<Series>>,
    cache: HashMap<u64, Arc<Series>>,
    models: BTreeMap<String, ModelRecord>,
}

impl Workspace {
    /// An empty workspace.
    #[must_use]
    pub fn new(knowledge: KnowledgePolicy) -> Self {
        Self { knowledge, ..Self::default() }
    }

    /// The knowledge policy.
    #[must_use]
    pub fn knowledge(&self) -> KnowledgePolicy {
        self.knowledge
    }

    /// Changes the knowledge policy. Model outputs are dropped: they are recomputed, or found in
    /// the cache, which is keyed by the policy.
    pub fn set_knowledge(&mut self, policy: KnowledgePolicy) {
        self.knowledge = policy;
        self.derived.clear();
    }

    /// Registers an input series.
    ///
    /// # Errors
    /// If the name is already used.
    pub fn add_series(&mut self, series: Series) -> Result<Arc<Series>> {
        let name = series.name().to_owned();
        if self.contains(&name) {
            return Err(Error::Invalid(format!("a series named {name} already exists")));
        }
        let r = Registered::new(series);
        self.registered.insert(name.clone(), r);
        self.series(&name).cloned()
    }

    /// Inserts or replaces a derived series (a model's or a book's output). A registered series is
    /// never replaced: every state has one producer.
    ///
    /// # Errors
    /// If `series` has the name of a registered series.
    pub fn put_derived(&mut self, series: Arc<Series>) -> Result<()> {
        if self.registered.contains_key(series.name()) {
            return Err(Error::Invalid(format!("a model's output would replace the registered series {}", series.name())));
        }
        self.derived.insert(series.name().to_owned(), series);
        Ok(())
    }

    /// Whether `name` is a registered series (data), rather than a model's output.
    #[must_use]
    pub fn is_registered(&self, name: &str) -> bool {
        self.registered.contains_key(name)
    }

    fn missing(&self, name: &str) -> Error {
        Error::Unknown(format!("no series named {name} (have {:?})", self.names()))
    }

    /// The series called `name`, as models and decisions see it (the information view).
    ///
    /// # Errors
    /// If there is no such series.
    pub fn series(&self, name: &str) -> Result<&Arc<Series>> {
        if let Some(r) = self.registered.get(name) {
            return Ok(match self.knowledge.information {
                Information::AsKnown => &r.raw,
                Information::FirstRelease => &r.first,
                Information::LegacyLatest => &r.latest,
            });
        }
        self.derived.get(name).ok_or_else(|| self.missing(name))
    }

    /// The series called `name`, as contracts step on it (the outcome view).
    ///
    /// # Errors
    /// If there is no such series.
    pub fn outcome(&self, name: &str) -> Result<&Arc<Series>> {
        if let Some(r) = self.registered.get(name) {
            return Ok(match self.knowledge.outcomes {
                Outcomes::AsBooked => &r.first,
                Outcomes::Restated => &r.latest,
            });
        }
        self.derived.get(name).ok_or_else(|| self.missing(name))
    }

    /// Whether a series called `name` exists.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.registered.contains_key(name) || self.derived.contains_key(name)
    }

    /// Names of all series, sorted.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.registered.keys().chain(self.derived.keys()).cloned().collect();
        v.sort();
        v
    }

    /// Every series and its producer, sorted by name (for the run manifest).
    #[must_use]
    pub fn producers(&self) -> Vec<(String, Producer)> {
        let mut v: Vec<(String, Producer)> = self
            .registered
            .keys()
            .map(|k| (k.clone(), Producer::Observed))
            .chain(self.derived.values().map(|s| (s.name().to_owned(), s.provenance().producer.clone())))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// A cached model output.
    #[must_use]
    pub fn cached(&self, key: u64) -> Option<Arc<Series>> {
        self.cache.get(&key).cloned()
    }

    /// Caches a model output under its content key.
    pub fn cache(&mut self, key: u64, series: Arc<Series>) {
        self.cache.insert(key, series);
    }

    /// Number of cached outputs.
    #[must_use]
    pub fn cache_len(&self) -> usize {
        self.cache.len()
    }

    /// Records what produced a model's outputs (the graph does, at each evaluation).
    pub fn record_model(&mut self, record: ModelRecord) {
        self.models.insert(record.name.clone(), record);
    }

    /// Every model whose outputs are in the workspace, by name.
    pub fn models(&self) -> impl Iterator<Item = &ModelRecord> {
        self.models.values()
    }
}
