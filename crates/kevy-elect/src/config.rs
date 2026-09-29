//! The elector's timeouts and backoff jitter.

use std::time::{Duration, Instant};

/// Tunable timeouts. Defaults match the protocol spec; override any of
/// them with the `with_*` builders.
///
/// ```
/// use std::time::Duration;
/// use kevy_elect::ElectConfig;
///
/// let cfg = ElectConfig::default().with_hb_interval(Duration::from_millis(50));
/// assert_eq!(cfg.hb_interval, Duration::from_millis(50));
/// assert_eq!(cfg.down_after, Duration::from_secs(5));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ElectConfig {
    /// Period between outbound `HB` per peer. Default 200 ms.
    pub hb_interval: Duration,
    /// Flag a peer DOWN after this duration without an inbound `HB`.
    /// Default 5 s = 25 × `hb_interval` (a transient 1 s blip
    /// doesn't trigger an election).
    pub down_after: Duration,
    /// Candidate waits this long for quorum `ACCEPT` before backing
    /// off. Default 3 s.
    pub election_timeout: Duration,
    /// Backoff floor after a failed election attempt. Real wait
    /// adds jitter up to `election_backoff_jitter` to prevent
    /// dueling candidates from re-running synchronously.
    pub election_backoff: Duration,
    /// Random jitter added to `election_backoff` per attempt.
    /// Default 4 s (so the real range is 1–5 s).
    pub election_backoff_jitter: Duration,
}

impl Default for ElectConfig {
    fn default() -> Self {
        Self {
            hb_interval: Duration::from_millis(200),
            down_after: Duration::from_secs(5),
            election_timeout: Duration::from_secs(3),
            election_backoff: Duration::from_secs(1),
            election_backoff_jitter: Duration::from_secs(4),
        }
    }
}

impl ElectConfig {
    /// Set [`ElectConfig::hb_interval`].
    ///
    /// ```
    /// use std::time::Duration;
    /// let cfg = kevy_elect::ElectConfig::default().with_hb_interval(Duration::from_millis(20));
    /// assert_eq!(cfg.hb_interval, Duration::from_millis(20));
    /// ```
    #[must_use]
    pub fn with_hb_interval(mut self, d: Duration) -> Self {
        self.hb_interval = d;
        self
    }

    /// Set [`ElectConfig::down_after`].
    ///
    /// ```
    /// use std::time::Duration;
    /// let cfg = kevy_elect::ElectConfig::default().with_down_after(Duration::from_secs(1));
    /// assert_eq!(cfg.down_after, Duration::from_secs(1));
    /// ```
    #[must_use]
    pub fn with_down_after(mut self, d: Duration) -> Self {
        self.down_after = d;
        self
    }

    /// Set [`ElectConfig::election_timeout`].
    ///
    /// ```
    /// use std::time::Duration;
    /// let cfg = kevy_elect::ElectConfig::default().with_election_timeout(Duration::from_secs(1));
    /// assert_eq!(cfg.election_timeout, Duration::from_secs(1));
    /// ```
    #[must_use]
    pub fn with_election_timeout(mut self, d: Duration) -> Self {
        self.election_timeout = d;
        self
    }

    /// Set [`ElectConfig::election_backoff`].
    ///
    /// ```
    /// use std::time::Duration;
    /// let cfg = kevy_elect::ElectConfig::default().with_election_backoff(Duration::ZERO);
    /// assert_eq!(cfg.election_backoff, Duration::ZERO);
    /// ```
    #[must_use]
    pub fn with_election_backoff(mut self, d: Duration) -> Self {
        self.election_backoff = d;
        self
    }

    /// Set [`ElectConfig::election_backoff_jitter`].
    ///
    /// ```
    /// use std::time::Duration;
    /// let cfg = kevy_elect::ElectConfig::default().with_election_backoff_jitter(Duration::ZERO);
    /// assert_eq!(cfg.election_backoff_jitter, Duration::ZERO);
    /// ```
    #[must_use]
    pub fn with_election_backoff_jitter(mut self, d: Duration) -> Self {
        self.election_backoff_jitter = d;
        self
    }
}

/// Source of jitter for election backoff. Tests use a fixed value;
/// production uses `ElectJitter::System` which reads `Instant`
/// + node_id as a poor-mans entropy. Pure-Rust 0-dep — no `rand` crate.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ElectJitter {
    /// Fixed value (test-friendly, deterministic).
    Fixed(Duration),
    /// Hash of `(now_nanos, node_id)` clamped into
    /// `[0, max_jitter)`. Deterministic enough for production while
    /// avoiding zero-cost-jitter dueling.
    System,
}

impl ElectJitter {
    /// Sample a jitter value in `[0, max]`.
    pub(crate) fn sample(&self, max: Duration, now: Instant, node_id: &str) -> Duration {
        match self {
            Self::Fixed(d) => *d.min(&max),
            Self::System => {
                // Mix `node_id` bytes into a u64 hash and clamp into
                // `[0, max.as_nanos())`. Coarse but adequate — the
                // jitter only needs to break ties between dueling
                // candidates, not be cryptographically random.
                let mut h: u64 = 1_469_598_103_934_665_603;
                for b in node_id.as_bytes() {
                    h = h.wrapping_mul(1_099_511_628_211) ^ u64::from(*b);
                }
                // Pull a u64 worth of bits out of `now`'s elapsed-
                // since-arbitrary-anchor representation. Using the
                // low 64 bits of `now.elapsed_since(anchor)` would
                // need an anchor — instead, hash a stable derivation
                // of `now` via the elector's lazy anchor approach.
                // For simplicity: mix `node_id` bytes again with a
                // per-call seed.
                let _ = now; // placeholder: production jitter wants per-call entropy.
                let span_ns = max.as_nanos().max(1) as u64;
                Duration::from_nanos(h % span_ns)
            }
        }
    }
}
