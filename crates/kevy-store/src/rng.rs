//! The engine's random source.
//!
//! There has been one here since the eviction sampler needed it — a splitmix32
//! buried in `evict.rs`, private, and seeded from the clock on each call. So
//! when SPOP and SRANDMEMBER needed randomness, they did not reach for it. They
//! returned the first members in hash-bucket order instead, which for a fixed
//! set is the same answer every single time.
//!
//! That is not a design decision anyone made. It is a gap that got written down
//! as a compatibility note and then left alone. `SPOP` exists to take an
//! ARBITRARY member — it is how you hand work out, sample a population, or
//! assign a bucket — and a deterministic SPOP breaks all three silently, which
//! is the worst way for something to break.
//!
//! So the generator moves here, becomes a real stream with state, and both
//! verbs use it. No OS entropy, no `getrandom`, nothing that would close the
//! `no_std` door: the state advances on every draw and is seeded from the
//! monotonic clock the store already keeps. It is not cryptographic and does not
//! need to be — nobody should be picking a session token with SPOP.

/// A splitmix64 stream. Small, fast, and good enough that a caller cannot
/// predict the next member of a set without doing arithmetic on purpose.
#[derive(Debug, Clone)]
pub(crate) struct Rng {
    state: u64,
}

impl Default for Rng {
    /// Seeded from the monotonic clock, so two runs of the same server do not
    /// hand out the same members in the same order. `Store` derives `Default`,
    /// which is why the seeding has to happen here rather than in a
    /// constructor nobody calls.
    fn default() -> Self {
        Self::new(crate::clock::now_ns())
    }
}

impl Rng {
    /// Seed from whatever monotonic value the caller has. Two stores that start
    /// in the same millisecond will draw the same sequence, which is fine — the
    /// contract is "arbitrary", not "unguessable".
    pub(crate) fn new(seed: u64) -> Self {
        // A zero seed makes splitmix64 degenerate; the golden-ratio constant is
        // the standard escape.
        Self {
            state: seed ^ 0x9E37_79B9_7F4A_7C15,
        }
    }

    pub(crate) fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

}

/// Uniform in `0..n` from one raw draw, without the modulo bias `% n` leaves.
///
/// Lemire's multiply-and-reject, minus the reject: a single draw, so the caller
/// can pre-collect its randomness before borrowing the value it is about to
/// shuffle. The residual bias is under one part in 2^64/n, which for a set is
/// invisible and for a coin flip would still be fine.
pub(crate) fn below(draw: u64, n: u64) -> u64 {
    if n <= 1 {
        return 0;
    }
    ((u128::from(draw) * u128::from(n)) >> 64) as u64
}

#[cfg(test)]
mod tests {
    use super::Rng;

    #[test]
    fn the_stream_advances() {
        let mut r = Rng::new(42);
        let a = r.next_u64();
        let b = r.next_u64();
        assert_ne!(a, b, "a generator that returns the same draw twice is a constant");
    }

    #[test]
    fn below_is_uniform_and_in_range() {
        let mut r = Rng::new(1);
        let mut seen = [0usize; 7];
        for _ in 0..7000 {
            let v = super::below(r.next_u64(), 7) as usize;
            assert!(v < 7);
            seen[v] += 1;
        }
        for c in seen {
            assert!(c > 800 && c < 1200, "{seen:?}");
        }
    }

    #[test]
    fn below_0_and_1_collapse_to_0() {
        assert_eq!(super::below(u64::MAX, 0), 0);
        assert_eq!(super::below(u64::MAX, 1), 0);
    }
}
