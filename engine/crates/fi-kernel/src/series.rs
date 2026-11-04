//! Series: typed, columnar values keyed by entity and event time, each with a knowledge time.
//!
//! Two layouts, one interface:
//!
//! * [`Layout::Rows`]: sparse rows sorted by (entity, event, known). Irregular times, several
//!   vintages of one value, relationships, events.
//! * [`Layout::Grid`]: a dense time-major grid (every time × every entity, one vintage). Marks,
//!   returns, model outputs. Cross-sections are contiguous slices.
//!
//! Every read takes a knowledge cut (`known_by`), so nothing can be read before it was known
//! (invariants I2 and I9). Missing values are NaN inside the core.

use std::collections::HashMap;

use crate::{
    Error, Result,
    entity::EntityId,
    hash::Hash64,
    time::{Calendar, Ts},
};

/// What a column's values mean. Unit conversions (percent to fraction, basis points) happen once,
/// at registration: inside the core every rate is a fraction.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Unit {
    /// A plain number.
    Number,
    /// A price (its convention, such as per 100, is part of the contract's definition).
    Price,
    /// A rate, as a fraction.
    Rate,
    /// A probability.
    Probability,
    /// A count.
    Count,
    /// A category code.
    Category,
    /// A 0/1 flag.
    Flag,
    /// A reference to another entity (an [`EntityId`] stored as a number).
    Ref,
}

impl Unit {
    fn code(self) -> u64 {
        self as u64
    }
}

/// One value column. Missing values are NaN.
#[derive(Clone, Debug)]
pub struct Column {
    /// Column name.
    pub name: String,
    /// What the values mean.
    pub unit: Unit,
    /// The values: one per row (rows layout) or time-major T × N (grid layout).
    pub values: Vec<f64>,
}

impl Column {
    /// A column.
    #[must_use]
    pub fn new(name: impl Into<String>, unit: Unit, values: Vec<f64>) -> Self {
        Self { name: name.into(), unit, values }
    }
}

/// How a series stores its rows.
#[derive(Clone, Debug)]
pub enum Layout {
    /// Sparse rows sorted by (entity, event, known).
    Rows {
        /// Entity per row.
        entity: Vec<EntityId>,
        /// Event time per row.
        event: Vec<Ts>,
        /// Knowledge time per row.
        known: Vec<Ts>,
        /// Row range per entity.
        ranges: HashMap<EntityId, (usize, usize)>,
    },
    /// A dense, time-major grid with one knowledge time per time.
    Grid {
        /// Entities, in column order.
        entities: Vec<EntityId>,
        /// Position of each entity.
        positions: HashMap<EntityId, usize>,
        /// Event times, strictly increasing.
        times: Vec<Ts>,
        /// Knowledge time per event time.
        known: Vec<Ts>,
    },
}

/// A value found by a read, with its event and knowledge times.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Hit {
    /// The value (never NaN).
    pub value: f64,
    /// When it is true.
    pub event: Ts,
    /// When it was known.
    pub known: Ts,
}

/// A named series.
#[derive(Clone, Debug)]
pub struct Series {
    name: String,
    layout: Layout,
    columns: Vec<Column>,
    version: u64,
    provenance: Provenance,
}

/// Which vintage of each (entity, event) a resolved series keeps.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Vintage {
    /// The first published value, with its knowledge time.
    First,
    /// The latest value, dated at the first publication.
    Latest,
}

/// How a series' knowledge times were established.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum KnowledgeBasis {
    /// From recorded publication times (a column of timestamps).
    Observed,
    /// From a declared assumption, described (a lag, a lag in rows, the event time).
    Assumed(String),
    /// Computed by a model from its inputs' knowledge times.
    Derived,
}

/// What a model is for: the roles are reported and checked separately (spec, change 6).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum ModelRole {
    /// A signal, a forecast or an analytic: the default.
    #[default]
    Signal,
    /// A real-world model of how the world evolves (the P measure).
    RealWorld,
    /// A pricing model: values from a state (the Q measure, or marks to model).
    Pricing,
    /// A named stress: a scenario override, not a probability.
    Stress,
}

/// Who produces a series (spec, change 4: every state has one producer).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Producer {
    /// Registered data: an observation.
    Observed,
    /// A model's output.
    Model {
        /// The model.
        name: String,
        /// Its role.
        role: ModelRole,
    },
    /// A book's output (a fund's NAV, a deal's flows).
    Book {
        /// The book.
        name: String,
    },
}

/// Why a series is not what happened (spec, change 4): it, or something upstream of it, was
/// intervened in. Every series downstream inherits it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Intervention {
    /// Values that did not happen: a named stress, or a scenario series a user supplied.
    Scenario(String),
    /// A decision other than the one taken: a manager's alternative policy, a different trade.
    Counterfactual(String),
    /// A stochastic model's world (spec, change 6).
    Simulation(String),
}

impl Intervention {
    /// `"scenario"`, `"counterfactual"` or `"simulation"`.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Scenario(_) => "scenario",
            Self::Counterfactual(_) => "counterfactual",
            Self::Simulation(_) => "simulation",
        }
    }

    /// What intervened.
    #[must_use]
    pub fn by(&self) -> &str {
        match self {
            Self::Scenario(s) | Self::Counterfactual(s) | Self::Simulation(s) => s,
        }
    }
}

/// Where a series comes from and what it describes.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Provenance {
    /// How its knowledge times were established.
    pub knowledge: KnowledgeBasis,
    /// Who produces it.
    pub producer: Producer,
    /// The state it describes, if declared (for example `"clo1.tranche_flows"`). An observation
    /// and a model may describe the same state; a run uses one of them (spec, change 4).
    pub describes: Option<String>,
    /// Why it is not what happened, if it is not.
    pub intervention: Option<Intervention>,
}

impl Default for Provenance {
    fn default() -> Self {
        Self { knowledge: KnowledgeBasis::Assumed("as constructed".into()), producer: Producer::Observed, describes: None, intervention: None }
    }
}

fn check_values(name: &str, columns: &[Column], len: usize) -> Result<()> {
    for c in columns {
        if c.values.len() != len {
            return Err(Error::Invalid(format!("series {name}: column {} has {} values, expected {len}", c.name, c.values.len())));
        }
        if let Some(i) = c.values.iter().position(|v| v.is_infinite()) {
            return Err(Error::Invalid(format!("series {name}: column {} has an infinite value at row {i}", c.name)));
        }
    }
    let mut seen = std::collections::HashSet::new();
    for c in columns {
        if !seen.insert(c.name.as_str()) {
            return Err(Error::Invalid(format!("series {name}: column {} appears twice", c.name)));
        }
    }
    Ok(())
}

impl Series {
    /// A rows series. Rows are sorted by (entity, event, known); a duplicate key is refused.
    ///
    /// # Errors
    /// Mismatched lengths, infinite values, duplicate columns or duplicate keys.
    pub fn rows(name: impl Into<String>, entity: Vec<EntityId>, event: Vec<Ts>, known: Vec<Ts>, columns: Vec<Column>) -> Result<Self> {
        let name = name.into();
        let n = entity.len();
        if event.len() != n || known.len() != n {
            return Err(Error::Invalid(format!("series {name}: entity, event and known lengths differ")));
        }
        check_values(&name, &columns, n)?;
        let mut idx: Vec<usize> = (0..n).collect();
        idx.sort_by_key(|&i| (entity[i], event[i], known[i]));
        let entity: Vec<EntityId> = idx.iter().map(|&i| entity[i]).collect();
        let event: Vec<Ts> = idx.iter().map(|&i| event[i]).collect();
        let known: Vec<Ts> = idx.iter().map(|&i| known[i]).collect();
        let columns: Vec<Column> = columns.into_iter().map(|c| Column { values: idx.iter().map(|&i| c.values[i]).collect(), ..c }).collect();
        for i in 1..n {
            if entity[i] == entity[i - 1] && event[i] == event[i - 1] && known[i] == known[i - 1] {
                return Err(Error::Invalid(format!("series {name}: duplicate row for entity {} at event {} known {}", entity[i].0, event[i], known[i])));
            }
        }
        let mut ranges = HashMap::new();
        let mut start = 0;
        for i in 1..=n {
            if i == n || entity[i] != entity[start] {
                if n > 0 {
                    ranges.insert(entity[start], (start, i));
                }
                start = i;
            }
        }
        let mut s = Self { name, layout: Layout::Rows { entity, event, known, ranges }, columns, version: 0, provenance: Provenance::default() };
        s.version = s.compute_version();
        Ok(s)
    }

    /// A grid series: `columns[c].values[t * N + e]` is entity `e` at time `t`.
    ///
    /// # Errors
    /// Duplicate entities, times not strictly increasing, or mismatched lengths.
    pub fn grid(name: impl Into<String>, entities: Vec<EntityId>, times: Vec<Ts>, known: Vec<Ts>, columns: Vec<Column>) -> Result<Self> {
        let name = name.into();
        if known.len() != times.len() {
            return Err(Error::Invalid(format!("series {name}: one knowledge time per time is required")));
        }
        if times.windows(2).any(|w| w[0] >= w[1]) {
            return Err(Error::Invalid(format!("series {name}: grid times must be strictly increasing")));
        }
        let mut positions = HashMap::with_capacity(entities.len());
        for (p, e) in entities.iter().enumerate() {
            if positions.insert(*e, p).is_some() {
                return Err(Error::Invalid(format!("series {name}: entity {} appears twice", e.0)));
            }
        }
        check_values(&name, &columns, times.len() * entities.len())?;
        let mut s = Self { name, layout: Layout::Grid { entities, positions, times, known }, columns, version: 0, provenance: Provenance::default() };
        s.version = s.compute_version();
        Ok(s)
    }

    fn compute_version(&self) -> u64 {
        let mut h = Hash64::new();
        h.str(&self.name);
        match &self.layout {
            Layout::Rows { entity, event, known, .. } => {
                h.u64(1);
                for i in 0..entity.len() {
                    h.u64(u64::from(entity[i].0));
                    h.i64(event[i].0);
                    h.i64(known[i].0);
                }
            }
            Layout::Grid { entities, times, known, .. } => {
                h.u64(2);
                for e in entities {
                    h.u64(u64::from(e.0));
                }
                for (t, k) in times.iter().zip(known) {
                    h.i64(t.0);
                    h.i64(k.0);
                }
            }
        }
        for c in &self.columns {
            h.str(&c.name);
            h.u64(c.unit.code());
            for v in &c.values {
                h.f64(*v);
            }
        }
        h.finish()
    }

    /// A renamed copy (same content, new version).
    #[must_use]
    pub fn renamed(&self, name: &str) -> Self {
        let mut s = self.clone();
        s.name = name.to_owned();
        s.version = s.compute_version();
        s
    }

    /// The name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The content version: equal content, equal version (invariant I10).
    #[must_use]
    pub fn version(&self) -> u64 {
        self.version
    }

    /// The layout.
    #[must_use]
    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// The columns.
    #[must_use]
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    /// Index of column `name`.
    ///
    /// # Errors
    /// If there is no such column.
    pub fn column(&self, name: &str) -> Result<usize> {
        self.columns.iter().position(|c| c.name == name).ok_or_else(|| {
            let have: Vec<&str> = self.columns.iter().map(|c| c.name.as_str()).collect();
            Error::Unknown(format!("series {} has no column {name} (has {have:?})", self.name))
        })
    }

    /// Number of stored values per column.
    #[must_use]
    pub fn len(&self) -> usize {
        match &self.layout {
            Layout::Rows { entity, .. } => entity.len(),
            Layout::Grid { entities, times, .. } => entities.len() * times.len(),
        }
    }

    /// Whether the series has no values.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The entities, ascending for rows, in column order for grids.
    #[must_use]
    pub fn entities(&self) -> Vec<EntityId> {
        match &self.layout {
            Layout::Rows { ranges, .. } => {
                let mut e: Vec<EntityId> = ranges.keys().copied().collect();
                e.sort_unstable();
                e
            }
            Layout::Grid { entities, .. } => entities.clone(),
        }
    }

    /// The distinct event times: the data's own calendar.
    #[must_use]
    pub fn calendar(&self) -> Calendar {
        match &self.layout {
            Layout::Rows { event, .. } => Calendar::new(event.clone()),
            Layout::Grid { times, .. } => Calendar::new(times.clone()),
        }
    }

    /// The value of column `col` for `e` with event time exactly `t`, known by `known_by`.
    /// With several vintages, the latest known by `known_by` wins.
    #[must_use]
    pub fn exact(&self, col: usize, e: EntityId, t: Ts, known_by: Ts) -> Option<Hit> {
        let values = &self.columns[col].values;
        match &self.layout {
            Layout::Rows { event, known, ranges, .. } => {
                let (s, en) = *ranges.get(&e)?;
                let hi = s + event[s..en].partition_point(|x| *x <= t);
                let mut i = hi;
                while i > s && event[i - 1] == t {
                    i -= 1;
                    if known[i] <= known_by && !values[i].is_nan() {
                        return Some(Hit { value: values[i], event: t, known: known[i] });
                    }
                }
                None
            }
            Layout::Grid { positions, times, known, entities, .. } => {
                let ti = times.binary_search(&t).ok()?;
                if known[ti] > known_by {
                    return None;
                }
                let p = *positions.get(&e)?;
                let v = values[ti * entities.len() + p];
                (!v.is_nan()).then_some(Hit { value: v, event: t, known: known[ti] })
            }
        }
    }

    /// The latest value of column `col` for `e` with event time at or before `t`, known by
    /// `known_by`: carry-forward semantics, for slow data such as financial statements.
    #[must_use]
    pub fn asof(&self, col: usize, e: EntityId, t: Ts, known_by: Ts) -> Option<Hit> {
        let values = &self.columns[col].values;
        match &self.layout {
            Layout::Rows { event, known, ranges, .. } => {
                let (s, en) = *ranges.get(&e)?;
                let hi = s + event[s..en].partition_point(|x| *x <= t);
                (s..hi).rev().find(|&i| known[i] <= known_by && !values[i].is_nan()).map(|i| Hit { value: values[i], event: event[i], known: known[i] })
            }
            Layout::Grid { positions, times, known, entities, .. } => {
                let p = *positions.get(&e)?;
                let n = entities.len();
                let hi = times.partition_point(|x| *x <= t);
                (0..hi).rev().find(|&ti| known[ti] <= known_by && !values[ti * n + p].is_nan()).map(|ti| Hit {
                    value: values[ti * n + p],
                    event: times[ti],
                    known: known[ti],
                })
            }
        }
    }

    /// Rows of entity `e` with event time in (`from`, `to`], known by `known_by` (rows layout):
    /// how a contract finds the flows of one interval. Returns row indices.
    #[must_use]
    pub fn rows_between(&self, e: EntityId, from: Ts, to: Ts, known_by: Ts) -> Vec<usize> {
        match &self.layout {
            Layout::Rows { event, known, ranges, .. } => {
                let Some(&(s, en)) = ranges.get(&e) else { return Vec::new() };
                let lo = s + event[s..en].partition_point(|x| *x <= from);
                let hi = s + event[s..en].partition_point(|x| *x <= to);
                (lo..hi).filter(|&i| known[i] <= known_by).collect()
            }
            Layout::Grid { .. } => Vec::new(),
        }
    }

    /// The value of column `col` at row `i` (rows layout).
    #[must_use]
    pub fn value(&self, col: usize, i: usize) -> f64 {
        self.columns[col].values[i]
    }

    /// The event time of row `i` (rows layout).
    #[must_use]
    pub fn row_event(&self, i: usize) -> Ts {
        match &self.layout {
            Layout::Rows { event, .. } => event[i],
            Layout::Grid { .. } => Ts::MIN,
        }
    }

    /// The knowledge time of row `i` (rows layout).
    #[must_use]
    pub fn row_known(&self, i: usize) -> Ts {
        match &self.layout {
            Layout::Rows { known, .. } => known[i],
            Layout::Grid { .. } => Ts::MIN,
        }
    }

    /// Grid dimensions (times, entities).
    #[must_use]
    pub fn grid_dims(&self) -> Option<(usize, usize)> {
        match &self.layout {
            Layout::Grid { times, entities, .. } => Some((times.len(), entities.len())),
            Layout::Rows { .. } => None,
        }
    }

    /// Index of event time `t` in a grid.
    #[must_use]
    pub fn grid_time_index(&self, t: Ts) -> Option<usize> {
        match &self.layout {
            Layout::Grid { times, .. } => times.binary_search(&t).ok(),
            Layout::Rows { .. } => None,
        }
    }

    /// Knowledge time of grid time index `ti`.
    #[must_use]
    pub fn grid_known(&self, ti: usize) -> Ts {
        match &self.layout {
            Layout::Grid { known, .. } => known[ti],
            Layout::Rows { .. } => Ts::MAX,
        }
    }

    /// The grid's entities, in column order.
    #[must_use]
    pub fn grid_entities(&self) -> Option<&[EntityId]> {
        match &self.layout {
            Layout::Grid { entities, .. } => Some(entities),
            Layout::Rows { .. } => None,
        }
    }

    /// The latest grid time index at or before `t` that was known by `known_by`.
    #[must_use]
    pub fn grid_latest_known(&self, t: Ts, known_by: Ts) -> Option<usize> {
        match &self.layout {
            Layout::Grid { times, known, .. } => {
                let hi = times.partition_point(|x| *x <= t);
                (0..hi).rev().find(|&ti| known[ti] <= known_by)
            }
            Layout::Rows { .. } => None,
        }
    }

    /// Column position of `e` in a grid.
    #[must_use]
    pub fn grid_position(&self, e: EntityId) -> Option<usize> {
        match &self.layout {
            Layout::Grid { positions, .. } => positions.get(&e).copied(),
            Layout::Rows { .. } => None,
        }
    }

    /// The cross-section of column `col` at grid time index `ti`: a contiguous slice.
    #[must_use]
    pub fn grid_row(&self, col: usize, ti: usize) -> &[f64] {
        let n = self.grid_dims().map_or(0, |d| d.1);
        &self.columns[col].values[ti * n..(ti + 1) * n]
    }

    /// Whether some (entity, event) has several vintages (a rows series with revisions).
    #[must_use]
    pub fn has_revisions(&self) -> bool {
        match &self.layout {
            Layout::Rows { entity, event, .. } => (1..entity.len()).any(|i| entity[i] == entity[i - 1] && event[i] == event[i - 1]),
            Layout::Grid { .. } => false,
        }
    }

    /// Where the series comes from.
    #[must_use]
    pub fn provenance(&self) -> &Provenance {
        &self.provenance
    }

    /// The same series with `provenance` (the content version is unchanged).
    #[must_use]
    pub fn with_provenance(mut self, provenance: Provenance) -> Self {
        self.provenance = provenance;
        self
    }

    /// One vintage per (entity, event).
    ///
    /// * `First` keeps the first published value and its knowledge time: point in time.
    /// * `Latest` keeps the latest value but dates it at the first publication: the revised value
    ///   appears as if it had been known all along, as the legacy frameworks use it.
    #[must_use]
    pub fn resolve(&self, vintage: Vintage) -> Self {
        let Layout::Rows { entity, event, known, .. } = &self.layout else {
            return self.clone();
        };
        let n = entity.len();
        let mut keep: Vec<(usize, Ts)> = Vec::with_capacity(n);
        let mut i = 0;
        while i < n {
            let mut j = i + 1;
            while j < n && entity[j] == entity[i] && event[j] == event[i] {
                j += 1;
            }
            let first_known = known[i];
            match vintage {
                Vintage::First => keep.push((i, first_known)),
                Vintage::Latest => keep.push((j - 1, first_known)),
            }
            i = j;
        }
        if keep.len() == n {
            return self.clone();
        }
        let columns =
            self.columns.iter().map(|c| Column { name: c.name.clone(), unit: c.unit, values: keep.iter().map(|(r, _)| c.values[*r]).collect() }).collect();
        Self::rows(
            self.name.clone(),
            keep.iter().map(|(r, _)| entity[*r]).collect(),
            keep.iter().map(|(r, _)| event[*r]).collect(),
            keep.iter().map(|(_, k)| *k).collect(),
            columns,
        )
        .expect("a resolved series has unique keys")
        .with_provenance(self.provenance.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(i: u32) -> EntityId {
        EntityId(i)
    }

    #[test]
    fn rows_reads_are_point_in_time() {
        // Entity 0: value 1 at t=10 known 11; revised to 2, known 20. Value 3 at t=30 known 31.
        let s = Series::rows(
            "x",
            vec![e(0), e(0), e(0)],
            vec![Ts(30), Ts(10), Ts(10)],
            vec![Ts(31), Ts(20), Ts(11)],
            vec![Column::new("v", Unit::Number, vec![3.0, 2.0, 1.0])],
        )
        .unwrap();
        assert_eq!(s.exact(0, e(0), Ts(10), Ts(15)).unwrap().value, 1.0);
        assert_eq!(s.exact(0, e(0), Ts(10), Ts(25)).unwrap().value, 2.0);
        assert!(s.exact(0, e(0), Ts(10), Ts(10)).is_none());
        assert_eq!(s.asof(0, e(0), Ts(29), Ts(100)).unwrap().value, 2.0);
        assert_eq!(s.asof(0, e(0), Ts(30), Ts(30)).unwrap().value, 2.0);
        assert_eq!(s.asof(0, e(0), Ts(30), Ts(31)).unwrap().value, 3.0);
        let first = s.resolve(Vintage::First);
        assert_eq!(first.exact(0, e(0), Ts(10), Ts::MAX).unwrap(), Hit { value: 1.0, event: Ts(10), known: Ts(11) });
        let latest = s.resolve(Vintage::Latest);
        assert_eq!(latest.exact(0, e(0), Ts(10), Ts::MAX).unwrap(), Hit { value: 2.0, event: Ts(10), known: Ts(11) });
    }

    #[test]
    fn duplicates_and_infinities_are_refused() {
        let dup = Series::rows("x", vec![e(0), e(0)], vec![Ts(1), Ts(1)], vec![Ts(1), Ts(1)], vec![Column::new("v", Unit::Number, vec![1.0, 2.0])]);
        assert!(matches!(dup, Err(Error::Invalid(_))));
        let inf = Series::rows("x", vec![e(0)], vec![Ts(1)], vec![Ts(1)], vec![Column::new("v", Unit::Number, vec![f64::INFINITY])]);
        assert!(matches!(inf, Err(Error::Invalid(_))));
    }

    #[test]
    fn grid_reads() {
        let s =
            Series::grid("g", vec![e(5), e(7)], vec![Ts(1), Ts(2)], vec![Ts(2), Ts(3)], vec![Column::new("v", Unit::Number, vec![1.0, 2.0, f64::NAN, 4.0])])
                .unwrap();
        assert_eq!(s.exact(0, e(7), Ts(2), Ts(3)).unwrap().value, 4.0);
        assert!(s.exact(0, e(7), Ts(2), Ts(2)).is_none());
        assert_eq!(s.asof(0, e(5), Ts(2), Ts(3)).unwrap().value, 1.0);
        let row = s.grid_row(0, 1);
        assert_eq!(row.len(), 2);
        assert!(row[0].is_nan());
        assert_eq!(row[1], 4.0);
    }
}
