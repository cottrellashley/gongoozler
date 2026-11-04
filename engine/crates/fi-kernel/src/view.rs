//! Views: what a decision may see at a time.

use std::sync::Arc;

use crate::{
    Result,
    entity::EntityId,
    series::{Hit, Series},
    time::Ts,
    workspace::Workspace,
};

/// Series values with event time at or before `t`, known by `known_by`.
#[derive(Clone, Copy)]
pub struct View<'a> {
    /// The workspace read from.
    pub ws: &'a Workspace,
    /// The latest event time visible.
    pub t: Ts,
    /// The knowledge cut.
    pub known_by: Ts,
}

impl<'a> View<'a> {
    /// A view at `t`, seeing what was known by `known_by`.
    #[must_use]
    pub fn new(ws: &'a Workspace, t: Ts, known_by: Ts) -> Self {
        Self { ws, t, known_by }
    }

    /// A named series.
    ///
    /// # Errors
    /// If there is no such series.
    pub fn series(&self, name: &str) -> Result<&'a Arc<Series>> {
        self.ws.series(name)
    }

    /// The value at exactly `t`.
    #[must_use]
    pub fn exact(&self, s: &Series, col: usize, e: EntityId) -> Option<Hit> {
        s.exact(col, e, self.t, self.known_by)
    }

    /// The latest value at or before `t`.
    #[must_use]
    pub fn asof(&self, s: &Series, col: usize, e: EntityId) -> Option<Hit> {
        s.asof(col, e, self.t, self.known_by)
    }

    /// The latest value at or before `t`, by names. `None` if absent.
    #[must_use]
    pub fn value(&self, series: &str, column: &str, e: EntityId) -> Option<f64> {
        let s = self.ws.series(series).ok()?;
        let c = s.column(column).ok()?;
        self.asof(s, c, e).map(|h| h.value)
    }

    /// One column for many entities, as known at this view: for a grid, the row at the latest time
    /// known (one lookup for the whole cross-section); for rows, each entity's latest value. NaN
    /// where an entity has no value.
    #[must_use]
    pub fn cross_section(&self, s: &Series, col: usize, entities: &[EntityId]) -> Vec<f64> {
        if let Some(grid) = s.grid_entities() {
            let Some(ti) = s.grid_latest_known(self.t, self.known_by) else {
                return vec![f64::NAN; entities.len()];
            };
            let row = s.grid_row(col, ti);
            if grid == entities {
                return row.to_vec();
            }
            return entities.iter().map(|e| s.grid_position(*e).map_or(f64::NAN, |p| row[p])).collect();
        }
        entities.iter().map(|e| self.asof(s, col, *e).map_or(f64::NAN, |h| h.value)).collect()
    }

    /// The entity `e` refers to through relationship column `col` of `rel`, as of `t`.
    #[must_use]
    pub fn via(&self, rel: &Series, col: usize, e: EntityId) -> Option<EntityId> {
        self.asof(rel, col, e).and_then(|h| EntityId::from_value(h.value))
    }
}
