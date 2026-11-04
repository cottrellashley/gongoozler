//! This machine against the one the speed budgets were set on (docs/spec.md, "Performance
//! budgets": 16 threads, 3 Nov 2026). A fixed single-threaded loop is timed here and compared
//! with its time there: a slower core scales every budget by that ratio, and fewer than 16 threads
//! scale the parallel ones by the threads missing. The bench and the Python speed tests both use
//! it, and print the factors, so a budget held on a small runner says what it was held to.

use std::time::Instant;

/// The calibration loop's best time on the machine the budgets were set on.
pub const REFERENCE_NS: f64 = 42_600_000.0;

/// The calibration loop's best of five times here, in nanoseconds: xorshift and floating point,
/// single-threaded, about 43 ms on the reference machine.
#[must_use]
pub fn calibration_ns() -> f64 {
    let mut best = f64::INFINITY;
    for _ in 0..5 {
        let t = Instant::now();
        let (mut x, mut acc) = (0x9E37_79B9_7F4A_7C15u64, 0.0f64);
        for _ in 0..(1 << 25) {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            acc += (x >> 11) as f64 * 1e-16;
        }
        std::hint::black_box(acc);
        best = best.min(t.elapsed().as_nanos() as f64);
    }
    best
}

/// The factors budgets scale by here: (single-threaded work, parallel work), never below one.
#[must_use]
pub fn factors(threads: usize) -> (f64, f64) {
    let core = (calibration_ns() / REFERENCE_NS).max(1.0);
    (core, core * (16.0 / threads.max(1) as f64).max(1.0))
}
