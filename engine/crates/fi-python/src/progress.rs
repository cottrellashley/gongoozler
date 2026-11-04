//! The engine's progress, into the watcher's ring when the run is supervised (review H6).
//!
//! Under `gongoozler run`, `GONGOOZLER_RING` names the directory the supervisor reads. On import,
//! the extension makes a ring of its own there (a ring has one writer: the Python emitter keeps
//! its own), says `hello`, and sets the kernel's progress sink. Records, ledgers and sweeps then
//! report as stages with progress, from Rust, while the run holds no Python lock. A report from a
//! worker thread skips rather than wait for the ring, and reports are kept to about twenty a
//! second per loop. A forked child does not write its parent's ring. At exit the ring is closed.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use fi_kernel::{Progress, Ts};
use fi_ring::{Ring, Value};
use pyo3::prelude::*;

/// Updates kept per loop: one every 50 ms, and the last.
const EVERY_S: f64 = 0.05;

struct Loop {
    start: Instant,
    last: Instant,
    unit: &'static str,
}

/// The kernel's progress sink, writing a ring.
pub struct RingSink {
    pid: u32,
    ring: Mutex<Ring>,
    loops: Mutex<HashMap<&'static str, Loop>>,
}

impl RingSink {
    fn mine(&self) -> bool {
        std::process::id() == self.pid
    }

    pub(crate) fn close(&self) {
        if self.mine()
            && let Ok(mut r) = self.ring.lock()
        {
            r.close();
        }
    }
}

impl Progress for RingSink {
    fn begin(&self, key: &'static str, total: u64, unit: &'static str) {
        if !self.mine() {
            return;
        }
        let now = Instant::now();
        if let Ok(mut l) = self.loops.lock() {
            l.insert(key, Loop { start: now, last: now, unit });
        }
        if let Ok(mut r) = self.ring.lock() {
            r.event("stage", &[("name", key.into()), ("phase", "begin".into()), ("depth", Value::Int(0))]);
            r.progress(key, 0.0, Some(total as f64), Some(unit), None, None, None);
        }
    }

    fn report(&self, key: &'static str, done: u64, total: u64, sim: Option<Ts>) {
        if !self.mine() {
            return;
        }
        let now = Instant::now();
        // The last update of a loop always lands; the others skip rather than wait.
        let last = done >= total;
        let (rate, unit) = {
            let mut l = match (last, self.loops.try_lock()) {
                (_, Ok(l)) => l,
                (true, Err(_)) => match self.loops.lock() {
                    Ok(l) => l,
                    Err(_) => return,
                },
                (false, Err(_)) => return,
            };
            let Some(lp) = l.get_mut(key) else { return };
            if done < total && now.duration_since(lp.last).as_secs_f64() < EVERY_S {
                return;
            }
            lp.last = now;
            (done as f64 / now.duration_since(lp.start).as_secs_f64().max(1e-9), lp.unit)
        };
        let eta = (rate > 0.0).then(|| (total.saturating_sub(done)) as f64 / rate);
        let ring = if last { self.ring.lock().ok() } else { self.ring.try_lock().ok() };
        if let Some(mut r) = ring {
            r.progress(key, done as f64, Some(total as f64), Some(unit), sim.map(|t| t.0), Some(rate), eta);
        }
    }

    fn end(&self, key: &'static str, ok: bool) {
        if !self.mine() {
            return;
        }
        let elapsed = self.loops.lock().ok().and_then(|mut l| l.remove(key)).map_or(0.0, |lp| lp.start.elapsed().as_secs_f64());
        if let Ok(mut r) = self.ring.lock() {
            r.event("stage", &[("name", key.into()), ("phase", "end".into()), ("depth", Value::Int(0)), ("elapsed", elapsed.into()), ("ok", ok.into())]);
        }
    }
}

static SINK: Mutex<Option<Arc<RingSink>>> = Mutex::new(None);

/// Under a supervisor, makes the engine's ring and sets the progress sink.
pub(crate) fn install() {
    let Some(dir) = std::env::var_os("GONGOOZLER_RING").filter(|d| !d.is_empty()) else { return };
    let run = std::env::var("GONGOOZLER_RUN_ID").ok().filter(|r| !r.is_empty());
    match Ring::create(Path::new(&dir), 1 << 20, 64, run) {
        Ok(mut ring) => {
            let pid = ring.pid();
            ring.event("hello", &[("producer", "fi-engine".into()), ("engine", env!("CARGO_PKG_VERSION").into())]);
            let sink = Arc::new(RingSink { pid, ring: Mutex::new(ring), loops: Mutex::new(HashMap::new()) });
            fi_kernel::progress::set(Some(sink.clone()));
            if let Ok(mut s) = SINK.lock() {
                *s = Some(sink);
            }
        }
        Err(e) => eprintln!("fi_engine: cannot open a ring in {} ({e}); the engine's progress is not reported", dir.to_string_lossy()),
    }
}

/// Closes the engine's ring: what is pending is written, and the supervisor sees a clean end.
#[pyfunction]
pub(crate) fn _close_progress() {
    fi_kernel::progress::set(None);
    if let Some(s) = SINK.lock().ok().and_then(|mut s| s.take()) {
        s.close();
    }
}
