//! The run's own records (spec, change 7).
//!
//! * A [`Manifest`] pins everything a result depends on besides the data's values, by version:
//!   - the series, with how their knowledge times were established and who produced them;
//!   - the knowledge policy;
//!   - the models, with their fingerprints, roles and vintages;
//!   - the calendar, the records and the book's policies and settings;
//!   - the mode, the approximations declared, and the output's version.
//!
//!   Two runs with equal manifests give the same output; [`Manifest::diff`] attributes any
//!   difference.
//! * Definitions declare what they model ([`fi_kernel::Contract::supports`]). [`support_matrix`]
//!   is generated from them, not maintained by hand. [`check_requirements`] refuses a run whose
//!   contracts do not model what it requires, unless it declares the gap an approximation, which
//!   the manifest then records.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::sync::Arc;

use fi_kernel::{
    Calendar, Contract, Error, KnowledgeBasis, LedgerOutput, Model, ModelRecord, Producer, Result, UnitRecords, Workspace, hash::Hash64, policy::Information,
    policy::Outcomes,
};

use crate::{Book, run_mode};

/// One series as the manifest pins it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeriesEntry {
    /// Name.
    pub name: String,
    /// Content version.
    pub version: u64,
    /// How its knowledge times were established.
    pub knowledge: String,
    /// Who produced it.
    pub producer: String,
    /// The state it describes.
    pub describes: Option<String>,
    /// Why it is not what happened, if it is not.
    pub intervention: Option<String>,
    /// Whether it holds revised values.
    pub revisions: bool,
}

/// Everything a run's result depends on besides the data's values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    /// The engine's version.
    pub engine: String,
    /// What decisions see of revised values, and which vintage contracts step on.
    pub knowledge: String,
    /// The calendar: times, first, last and version.
    pub calendar: (usize, i64, i64, u64),
    /// Every series in the workspace, by name.
    pub series: Vec<SeriesEntry>,
    /// Every model whose outputs are in the workspace, by name.
    pub models: Vec<ModelRecord>,
    /// The unit records' version.
    pub records: u64,
    /// The book: name and fingerprint.
    pub book: (String, u64),
    /// Each policy: name, decision, and the decision's parameters.
    pub policies: Vec<(String, String, u64)>,
    /// The execution model.
    pub execution: String,
    /// The book's settings, written out.
    pub settings: String,
    /// The run's mode, derived from what it substitutes and intervenes.
    pub mode: String,
    /// Approximations the run declared.
    pub approximations: Vec<String>,
    /// The output's version (not part of the fingerprint: it is what the rest determines).
    pub output: u64,
}

fn knowledge(b: &KnowledgeBasis) -> String {
    match b {
        KnowledgeBasis::Observed => "observed".into(),
        KnowledgeBasis::Assumed(how) => format!("assumed: {how}"),
        KnowledgeBasis::Derived => "derived".into(),
    }
}

fn producer(p: &Producer) -> String {
    match p {
        Producer::Observed => "observation".into(),
        Producer::Model { name, role } => format!("model {name} ({role:?})"),
        Producer::Book { name } => format!("book {name}"),
    }
}

impl Manifest {
    /// The manifest of a run: `out` from `book` on `records` (whose contracts read `reads`) over
    /// the workspace `ws`.
    #[must_use]
    pub fn of(ws: &Workspace, records: &UnitRecords, reads: &[String], book: &Book, out: &LedgerOutput, approximations: &[String]) -> Self {
        let k = ws.knowledge();
        let info = match k.information {
            Information::AsKnown => "as_known",
            Information::FirstRelease => "first_release",
            Information::LegacyLatest => "legacy_latest",
        };
        let outcomes = match k.outcomes {
            Outcomes::AsBooked => "as_booked",
            Outcomes::Restated => "restated",
        };
        let cal: &Calendar = records.calendar();
        let times = cal.times();
        let series = ws
            .names()
            .into_iter()
            .filter_map(|n| ws.series(&n).ok().map(|s| (n, s.clone())))
            .map(|(name, s)| {
                let p = s.provenance();
                SeriesEntry {
                    name,
                    version: s.version(),
                    knowledge: knowledge(&p.knowledge),
                    producer: producer(&p.producer),
                    describes: p.describes.clone(),
                    intervention: p.intervention.as_ref().map(|i| format!("{} by {}", i.kind(), i.by())),
                    revisions: s.has_revisions(),
                }
            })
            .collect();
        Self {
            engine: env!("CARGO_PKG_VERSION").to_owned(),
            knowledge: format!("information {info}, outcomes {outcomes}"),
            calendar: (times.len(), times.first().map_or(0, |t| t.0), times.last().map_or(0, |t| t.0), cal.version()),
            series,
            models: ws.models().cloned().collect(),
            records: records.version(),
            book: (book.name.clone(), book.fingerprint()),
            policies: book.policies.iter().map(|p| (p.name.clone(), p.decide.name(), p.decide.params())).collect(),
            execution: match &book.execution {
                crate::Executor::Fill(f) => format!("fill {}", f.name()),
                crate::Executor::Process(e) => format!("process {}", e.name()),
            },
            settings: format!("{:?}", book.config),
            mode: run_mode(ws, reads, book).to_string(),
            approximations: approximations.to_vec(),
            output: out.version(),
        }
    }

    /// A hash of everything that determines the output.
    #[must_use]
    pub fn fingerprint(&self) -> u64 {
        let mut h = Hash64::new();
        let pinned = Self { output: 0, ..self.clone() };
        h.str(&format!("{pinned:?}"));
        h.finish()
    }

    /// What differs from `other`, attributed: each series, model, setting and output that changed.
    #[must_use]
    pub fn diff(&self, other: &Self) -> Vec<String> {
        let mut d = Vec::new();
        let mut field = |what: &str, a: String, b: String| {
            if a != b {
                d.push(format!("{what}: {a} -> {b}"));
            }
        };
        field("engine", self.engine.clone(), other.engine.clone());
        field("knowledge", self.knowledge.clone(), other.knowledge.clone());
        field("calendar", format!("{:?}", self.calendar), format!("{:?}", other.calendar));
        field("records", self.records.to_string(), other.records.to_string());
        field("book", format!("{:?}", self.book), format!("{:?}", other.book));
        field("policies", format!("{:?}", self.policies), format!("{:?}", other.policies));
        field("execution", self.execution.clone(), other.execution.clone());
        field("settings", self.settings.clone(), other.settings.clone());
        field("mode", self.mode.clone(), other.mode.clone());
        field("approximations", format!("{:?}", self.approximations), format!("{:?}", other.approximations));
        let names: BTreeSet<&str> = self.series.iter().chain(&other.series).map(|s| s.name.as_str()).collect();
        for n in names {
            let (a, b) = (self.series.iter().find(|s| s.name == n), other.series.iter().find(|s| s.name == n));
            match (a, b) {
                (Some(a), Some(b)) if a != b => d.push(format!("series {n}: {a:?} -> {b:?}")),
                (Some(_), None) => d.push(format!("series {n}: removed")),
                (None, Some(_)) => d.push(format!("series {n}: added")),
                _ => {}
            }
        }
        let names: BTreeSet<&str> = self.models.iter().chain(&other.models).map(|m| m.name.as_str()).collect();
        for n in names {
            let (a, b) = (self.models.iter().find(|m| m.name == n), other.models.iter().find(|m| m.name == n));
            match (a, b) {
                (Some(a), Some(b)) if a.fingerprint != b.fingerprint || a.vintages != b.vintages => {
                    d.push(format!("model {n}: fingerprint {} -> {}, vintages {:?} -> {:?}", a.fingerprint, b.fingerprint, a.vintages, b.vintages));
                }
                (Some(a), Some(b)) if a != b => d.push(format!("model {n}: inputs changed (key {} -> {})", a.key, b.key)),
                (Some(_), None) => d.push(format!("model {n}: removed")),
                (None, Some(_)) => d.push(format!("model {n}: added")),
                _ => {}
            }
        }
        if self.output != other.output {
            d.push(format!("output: {} -> {}", self.output, other.output));
        }
        d
    }

    /// The manifest as JSON: its serialised form.
    #[must_use]
    pub fn to_json(&self) -> String {
        let q = |s: &str| {
            let mut o = String::with_capacity(s.len() + 2);
            o.push('"');
            for c in s.chars() {
                match c {
                    '"' => o.push_str("\\\""),
                    '\\' => o.push_str("\\\\"),
                    '\n' => o.push_str("\\n"),
                    c if (c as u32) < 0x20 => {
                        let _ = write!(o, "\\u{:04x}", c as u32);
                    }
                    c => o.push(c),
                }
            }
            o.push('"');
            o
        };
        let opt = |s: &Option<String>| s.as_deref().map_or_else(|| "null".to_owned(), q);
        let mut j = String::new();
        let _ = write!(j, "{{\"engine\":{},\"knowledge\":{},", q(&self.engine), q(&self.knowledge));
        let (n, first, last, v) = self.calendar;
        let _ = write!(j, "\"calendar\":{{\"times\":{n},\"first\":{first},\"last\":{last},\"version\":\"{v:016x}\"}},");
        j.push_str("\"series\":[");
        for (i, s) in self.series.iter().enumerate() {
            if i > 0 {
                j.push(',');
            }
            let _ = write!(
                j,
                "{{\"name\":{},\"version\":\"{:016x}\",\"knowledge\":{},\"producer\":{},\"describes\":{},\"intervention\":{},\"revisions\":{}}}",
                q(&s.name),
                s.version,
                q(&s.knowledge),
                q(&s.producer),
                opt(&s.describes),
                opt(&s.intervention),
                s.revisions
            );
        }
        j.push_str("],\"models\":[");
        for (i, m) in self.models.iter().enumerate() {
            if i > 0 {
                j.push(',');
            }
            let vintages: Vec<String> = m.vintages.iter().map(|(t, f)| format!("[{},\"{f:016x}\"]", t.0)).collect();
            let _ = write!(
                j,
                "{{\"name\":{},\"fingerprint\":\"{:016x}\",\"key\":\"{:016x}\",\"role\":{},\"available_from\":{},\"retrospective\":{},\"vintages\":[{}],\"writes\":[{}],\"intervention\":{}}}",
                q(&m.name),
                m.fingerprint,
                m.key,
                q(&format!("{:?}", m.role)),
                m.available_from.map_or_else(|| "null".to_owned(), |t| t.0.to_string()),
                m.retrospective,
                vintages.join(","),
                m.writes.iter().map(|w| q(w)).collect::<Vec<_>>().join(","),
                opt(&m.intervention.as_ref().map(|i| format!("{} by {}", i.kind(), i.by()))),
            );
        }
        let _ = write!(j, "],\"records\":\"{:016x}\",\"book\":{{\"name\":{},\"fingerprint\":\"{:016x}\"}},", self.records, q(&self.book.0), self.book.1);
        j.push_str("\"policies\":[");
        for (i, (n, d, p)) in self.policies.iter().enumerate() {
            if i > 0 {
                j.push(',');
            }
            let _ = write!(j, "{{\"name\":{},\"decision\":{},\"params\":\"{p:016x}\"}}", q(n), q(d));
        }
        let approx: Vec<String> = self.approximations.iter().map(|a| q(a)).collect();
        let _ = write!(
            j,
            "],\"execution\":{},\"settings\":{},\"mode\":{},\"approximations\":[{}],\"output\":\"{:016x}\",\"fingerprint\":\"{:016x}\"}}",
            q(&self.execution),
            q(&self.settings),
            q(&self.mode),
            approx.join(","),
            self.output,
            self.fingerprint()
        );
        j
    }
}

/// What each definition models, generated from the definitions themselves: (kind, feature),
/// sorted.
#[must_use]
pub fn support_matrix(contracts: &[Arc<dyn Contract>], models: &[Model]) -> Vec<(String, String)> {
    let mut m: BTreeSet<(String, String)> = BTreeSet::new();
    for c in contracts {
        for f in c.supports() {
            m.insert((c.kind().to_owned(), f.to_owned()));
        }
    }
    for x in models {
        if let Model::Batch(b) = x {
            for f in b.supports() {
                m.insert((format!("model {}", b.spec().name), f.to_owned()));
            }
        }
    }
    m.into_iter().collect()
}

/// Checks that every contract of each kind in `requires` models each required feature. A gap
/// listed in `approximate` is allowed and returned, for the manifest; any other is refused.
///
/// # Errors
/// The first contract that does not model a required feature.
pub fn check_requirements(
    ws: &Workspace,
    contracts: &[Arc<dyn Contract>],
    requires: &[(String, String)],
    approximate: &[(String, String)],
) -> Result<Vec<String>> {
    let mut used = BTreeSet::new();
    for c in contracts {
        let has = c.supports();
        for (kind, feature) in requires {
            if kind != c.kind() || has.contains(&feature.as_str()) {
                continue;
            }
            if approximate.iter().any(|(k, f)| k == kind && f == feature) {
                used.insert(format!("{kind} does not model {feature}: approximated, as declared"));
            } else {
                return Err(Error::Invalid(format!(
                    "{} ({kind}) does not model {feature}: use a definition that does, or declare it an approximation",
                    ws.entities.name(c.entity())
                )));
            }
        }
    }
    Ok(used.into_iter().collect())
}
