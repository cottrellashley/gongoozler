//! This machine against the one the speed budgets were set on (docs/spec.md, "Performance
//! budgets": 16 threads, 3 Nov 2026). Two fixed single-threaded loops, one of arithmetic and one of
//! memory latency, are timed here and compared with their times there: a slower core scales every
//! budget by the larger ratio, and fewer than 16 threads scale the parallel ones by the threads
//! missing. The bench and the Python speed tests both use it, and print the factors, so a budget
//! held on a small runner says what it was held to. The loops are not the engine: on CI's runners
//! the engine slowed by up to 1.5× more than they did (kb/measurements.md at the repository's
//! root), which is why the bench takes a tolerance there.

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

/// The memory loop's best time on the machine the budgets were set on.
pub const MEMORY_REFERENCE_NS: f64 = 42_000_000.0;

/// Memory latency: half a million dependent loads through a random cycle over 32 MiB, larger than
/// the caches, as work over records that outgrow them pays. The best of three.
#[must_use]
pub fn memory_ns() -> f64 {
    let n = 1usize << 22;
    // A single cycle through every slot (Sattolo's shuffle), from a fixed seed.
    let mut next: Vec<u64> = (0..n as u64).collect();
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    for i in (1..n).rev() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        next.swap(i, (x % i as u64) as usize);
    }
    let mut best = f64::INFINITY;
    for _ in 0..3 {
        let t = Instant::now();
        let mut p = 0usize;
        for _ in 0..(1 << 19) {
            p = next[p] as usize;
        }
        std::hint::black_box(p);
        best = best.min(t.elapsed().as_nanos() as f64);
    }
    best
}

/// The factors budgets scale by here: (single-threaded work, parallel work), never below one.
/// A core's factor is the slower of its arithmetic and its memory against the reference machine's,
/// since the engine's loops are bound by either, as their data fit the caches or not.
#[must_use]
pub fn factors(threads: usize) -> (f64, f64) {
    let core = (calibration_ns() / REFERENCE_NS).max(memory_ns() / MEMORY_REFERENCE_NS).max(1.0);
    (core, core * (16.0 / threads.max(1) as f64).max(1.0))
}
