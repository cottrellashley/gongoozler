//! Keyed randomness (spec, change 6). Every draw is a pure function of its key: the stream's seed
//! and namespace, the world, the component, the entity and the event. A world's draws therefore
//! do not depend on evaluation order or threads, a re-run reproduces them, and two policies
//! compared on the same world see the same draws (common random numbers).

use crate::hash::Hash64;

/// A counter-based stream of draws.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Stream {
    seed: u64,
    namespace: u64,
}

/// SplitMix64's finaliser: a bijective mix of 64 bits.
fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

impl Stream {
    /// A stream for `namespace` (a model's name) under `seed`.
    #[must_use]
    pub fn new(seed: u64, namespace: &str) -> Self {
        let mut h = Hash64::new();
        h.str(namespace);
        Self { seed, namespace: h.finish() }
    }

    fn bits(&self, world: u64, component: u64, entity: u64, event: i64) -> u64 {
        let mut z = mix(self.seed ^ 0x9e37_79b9_7f4a_7c15);
        for part in [self.namespace, world, component, entity, event as u64] {
            z = mix(z ^ part.wrapping_add(0x9e37_79b9_7f4a_7c15));
        }
        z
    }

    /// A uniform draw in (0, 1).
    #[must_use]
    pub fn uniform(&self, world: u64, component: u64, entity: u64, event: i64) -> f64 {
        ((self.bits(world, component, entity, event) >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }

    /// A standard normal draw (Box–Muller on two keyed uniforms).
    #[must_use]
    pub fn normal(&self, world: u64, component: u64, entity: u64, event: i64) -> f64 {
        let u1 = self.uniform(world, component.wrapping_mul(2), entity, event);
        let u2 = self.uniform(world, component.wrapping_mul(2).wrapping_add(1), entity, event);
        (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }
}

#[cfg(test)]
mod tests {
    use super::Stream;

    #[test]
    fn draws_are_keyed_and_well_spread() {
        let s = Stream::new(7, "rates");
        assert_eq!(s.uniform(1, 2, 3, 4), s.uniform(1, 2, 3, 4));
        assert_ne!(s.uniform(1, 2, 3, 4), s.uniform(2, 2, 3, 4));
        assert_ne!(s.uniform(1, 2, 3, 4), Stream::new(7, "credit").uniform(1, 2, 3, 4));
        let n = 20_000;
        let z: Vec<f64> = (0..n).map(|i| s.normal(0, 0, 0, i)).collect();
        let mean = z.iter().sum::<f64>() / n as f64;
        let var = z.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n as f64;
        assert!(mean.abs() < 0.03, "{mean}");
        assert!((var - 1.0).abs() < 0.05, "{var}");
        let u: Vec<f64> = (0..n).map(|i| s.uniform(9, 1, 1, i)).collect();
        assert!(u.iter().all(|x| *x > 0.0 && *x < 1.0));
        assert!((u.iter().sum::<f64>() / n as f64 - 0.5).abs() < 0.01);
    }
}
