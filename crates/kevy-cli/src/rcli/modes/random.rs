//! Pseudo-random numbers for load and query generators: xorshift64*, seeded
//! from the clock and the process id. Not for anything secret.

/// A generator; each mode keeps its own.
pub(crate) struct Random(u64);

impl Random {
    pub(crate) fn seeded() -> Random {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        Random((nanos ^ u64::from(std::process::id()).rotate_left(32)) | 1)
    }

    pub(crate) fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// Uniform in `[0, 1)`.
    pub(crate) fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

#[cfg(test)]
mod tests {
    use super::Random;

    #[test]
    fn units_stay_in_range() {
        let mut r = Random::seeded();
        assert!((0..10_000).map(|_| r.unit()).all(|u| (0.0..1.0).contains(&u)));
        assert_ne!(r.next_u64(), r.next_u64());
    }
}
