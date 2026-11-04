//! A small, stable 64-bit content hash for versions and cache keys.
//!
//! It is stable across runs and Rust releases (unlike `std`'s `DefaultHasher`), so a series'
//! version, a cached output's key and a run's fingerprint can be compared between processes.
//! It is not cryptographic.

/// Word-at-a-time mixing hash.
#[derive(Clone, Debug)]
pub struct Hash64(u64);

const K: u64 = 0x9E37_79B9_7F4A_7C15;

impl Default for Hash64 {
    fn default() -> Self {
        Self::new()
    }
}

impl Hash64 {
    /// A fresh hash.
    #[must_use]
    pub const fn new() -> Self {
        Self(0x243F_6A88_85A3_08D3)
    }

    /// Mix in one 64-bit word.
    #[inline]
    pub fn u64(&mut self, x: u64) {
        self.0 = (self.0 ^ x).wrapping_mul(K).rotate_left(29) ^ (self.0 >> 31);
    }

    /// Mix in a signed integer.
    #[inline]
    pub fn i64(&mut self, x: i64) {
        self.u64(x as u64);
    }

    /// Mix in a float by its bits (so NaN payloads and signed zeros count).
    #[inline]
    pub fn f64(&mut self, x: f64) {
        self.u64(x.to_bits());
    }

    /// Mix in a string (length first, so concatenations differ).
    pub fn str(&mut self, s: &str) {
        self.u64(s.len() as u64);
        for chunk in s.as_bytes().chunks(8) {
            let mut b = [0u8; 8];
            b[..chunk.len()].copy_from_slice(chunk);
            self.u64(u64::from_le_bytes(b));
        }
    }

    /// The hash.
    #[must_use]
    pub fn finish(&self) -> u64 {
        let mut x = self.0;
        x ^= x >> 33;
        x = x.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
        x ^= x >> 33;
        x
    }
}

#[cfg(test)]
mod tests {
    use super::Hash64;

    #[test]
    fn stable_and_sensitive() {
        let mut a = Hash64::new();
        a.str("abc");
        a.f64(1.0);
        let mut b = Hash64::new();
        b.str("abc");
        b.f64(1.0);
        assert_eq!(a.finish(), b.finish());
        let mut c = Hash64::new();
        c.str("abd");
        c.f64(1.0);
        assert_ne!(a.finish(), c.finish());
    }
}
