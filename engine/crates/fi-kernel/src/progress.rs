//! Progress from the engine's long loops, for whoever watches the run (review H6).
//!
//! Computing unit records, stepping a ledger and running a sweep each report how far they have got
//! to the process's sink, if one is set: the watcher's ring when the run is supervised
//! (`fi-python` sets it from `GONGOOZLER_RING`), nothing otherwise. A sink is called from the
//! loop's own thread, so it must never block; the ring's sink skips an update rather than wait.

use std::sync::{Arc, RwLock};

use crate::time::Ts;

/// Where progress goes.
pub trait Progress: Send + Sync {
    /// A loop over `total` `unit`s starts.
    fn begin(&self, key: &'static str, total: u64, unit: &'static str);
    /// It has done `done` of `total`, and reached simulated time `sim`.
    fn report(&self, key: &'static str, done: u64, total: u64, sim: Option<Ts>);
    /// It has ended, well or not.
    fn end(&self, key: &'static str, ok: bool);
}

static SINK: RwLock<Option<Arc<dyn Progress>>> = RwLock::new(None);

/// Sets the process's sink (`None`: no progress).
pub fn set(sink: Option<Arc<dyn Progress>>) {
    if let Ok(mut s) = SINK.write() {
        *s = sink;
    }
}

/// The process's sink, if any.
#[must_use]
pub fn sink() -> Option<Arc<dyn Progress>> {
    SINK.read().ok().and_then(|s| s.clone())
}

/// A loop's reporting: begins on creation, ends (not ok) on drop unless finished.
pub struct Reporter {
    sink: Option<Arc<dyn Progress>>,
    key: &'static str,
    total: u64,
    ended: bool,
}

impl Reporter {
    /// Starts reporting `key`, over `total` `unit`s, if a sink is set.
    #[must_use]
    pub fn start(key: &'static str, total: u64, unit: &'static str) -> Self {
        let sink = sink();
        if let Some(s) = &sink {
            s.begin(key, total, unit);
        }
        Self { sink, key, total, ended: false }
    }

    /// A reporter that reports nothing (a ledger inside a sweep, which reports the sweep).
    #[must_use]
    pub fn silent() -> Self {
        Self { sink: None, key: "", total: 0, ended: true }
    }

    /// `done` of the total, at `sim`.
    pub fn report(&self, done: u64, sim: Option<Ts>) {
        if let Some(s) = &self.sink {
            s.report(self.key, done, self.total, sim);
        }
    }

    /// The loop ended.
    pub fn finish(mut self, ok: bool) {
        self.end(ok);
    }

    fn end(&mut self, ok: bool) {
        if !self.ended {
            self.ended = true;
            if let Some(s) = &self.sink {
                s.end(self.key, ok);
            }
        }
    }
}

impl Drop for Reporter {
    fn drop(&mut self) {
        self.end(false);
    }
}
