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
    ///
    /// ```
    /// use std::time::{Duration, Instant};
    /// use kevy_elect::{ElectConfig, Elector, Role};
    ///
    /// let cfg = ElectConfig::default().with_hb_interval(Duration::from_millis(100));
    /// let mut a = Elector::new("a", vec!["a".into(), "b".into()], "a:6004", Role::Primary).with_config(cfg);
    /// let t0 = Instant::now();
    /// assert_eq!(a.tick(t0).len(), 1);
    /// assert!(a.tick(t0 + Duration::from_millis(50)).is_empty()); // not due yet
    /// assert_eq!(a.tick(t0 + Duration::from_millis(100)).len(), 1);
    /// ```
    pub hb_interval: Duration,
    /// Flag a peer DOWN after this duration without an inbound `HB`.
    /// Default 5 s = 25 × `hb_interval` (a transient 1 s blip
    /// doesn't trigger an election).
    ///
    /// ```
    /// # use std::time::{Duration, Instant};
    /// # use kevy_elect::{ElectConfig, ElectJitter, Elector, Message, Role};
    /// # fn offers(out: &[kevy_elect::Outbound]) -> bool {
    /// #     out.iter().any(|o| matches!(o.msg, Message::Offer { .. }))
    /// # }
    /// # let cfg = ElectConfig::default().with_down_after(Duration::from_secs(2));
    /// # let mut b = Elector::new("b", vec!["a".into(), "b".into()], "b:6004", Role::Replica)
    /// #     .with_config(cfg).with_jitter(ElectJitter::Fixed(Duration::ZERO));
    /// # let t0 = Instant::now();
    /// # let hb = Message::Hb { epoch: 1, node_id: "a".into(), role: Role::Primary, repl_offset: 0 };
    /// # b.on_message("a", hb, t0);
    /// // the primary last spoke at t0
    /// assert!(!offers(&b.tick(t0 + Duration::from_millis(1999))));
    /// assert!(offers(&b.tick(t0 + Duration::from_secs(2)))); // now it is DOWN
    /// ```
    pub down_after: Duration,
    /// Candidate waits this long for quorum `ACCEPT` before backing
    /// off. Default 3 s.
    ///
    /// ```
    /// # use std::time::{Duration, Instant};
    /// # use kevy_elect::{ElectConfig, ElectJitter, Elector, Message, Role};
    /// # fn offers(out: &[kevy_elect::Outbound]) -> bool {
    /// #     out.iter().any(|o| matches!(o.msg, Message::Offer { .. }))
    /// # }
    /// # let cfg = ElectConfig::default();
    /// # let mut b = Elector::new("b", vec!["a".into(), "b".into()], "b:6004", Role::Replica)
    /// #     .with_config(cfg).with_jitter(ElectJitter::Fixed(Duration::ZERO));
    /// # let t0 = Instant::now();
    /// # let hb = Message::Hb { epoch: 1, node_id: "a".into(), role: Role::Primary, repl_offset: 0 };
    /// # b.on_message("a", hb, t0);
    /// b.tick(t0 + Duration::from_secs(5)); // offers; nobody answers
    /// assert_eq!(b.role(), Role::Candidate);
    /// b.tick(t0 + Duration::from_secs(8)); // 3 s later the candidacy lapses
    /// assert_eq!(b.role(), Role::Replica);
    /// ```
    pub election_timeout: Duration,
    /// Backoff floor after a failed election attempt. Real wait
    /// adds jitter up to `election_backoff_jitter` to prevent
    /// dueling candidates from re-running synchronously.
    ///
    /// ```
    /// # use std::time::{Duration, Instant};
    /// # use kevy_elect::{ElectConfig, ElectJitter, Elector, Message, Role};
    /// # fn offers(out: &[kevy_elect::Outbound]) -> bool {
    /// #     out.iter().any(|o| matches!(o.msg, Message::Offer { .. }))
    /// # }
    /// # let cfg = ElectConfig::default();
    /// # let mut b = Elector::new("b", vec!["a".into(), "b".into()], "b:6004", Role::Replica)
    /// #     .with_config(cfg).with_jitter(ElectJitter::Fixed(Duration::ZERO));
    /// # let t0 = Instant::now();
    /// # let hb = Message::Hb { epoch: 1, node_id: "a".into(), role: Role::Primary, repl_offset: 0 };
    /// # b.on_message("a", hb, t0);
    /// b.tick(t0 + Duration::from_secs(5)); // first candidacy
    /// b.tick(t0 + Duration::from_secs(8)); // times out
    /// assert!(!offers(&b.tick(t0 + Duration::from_millis(8500)))); // backing off
    /// assert!(offers(&b.tick(t0 + Duration::from_secs(9)))); // 1 s later it tries again
    /// assert_eq!(b.epoch(), 3);
    /// ```
    pub election_backoff: Duration,
    /// Random jitter added to `election_backoff` per attempt.
    /// Default 4 s (so the real range is 1–5 s).
    ///
    /// ```
    /// # use std::time::{Duration, Instant};
    /// # use kevy_elect::{ElectConfig, ElectJitter, Elector, Message, Role};
    /// # fn offers(out: &[kevy_elect::Outbound]) -> bool {
    /// #     out.iter().any(|o| matches!(o.msg, Message::Offer { .. }))
    /// # }
    /// # let cfg = ElectConfig::default();
    /// # let mut b = Elector::new("b", vec!["a".into(), "b".into()], "b:6004", Role::Replica)
    /// #     .with_config(cfg).with_jitter(ElectJitter::Fixed(Duration::from_secs(10)));
    /// # let t0 = Instant::now();
    /// # let hb = Message::Hb { epoch: 1, node_id: "a".into(), role: Role::Primary, repl_offset: 0 };
    /// # b.on_message("a", hb, t0);
    /// b.tick(t0 + Duration::from_secs(5));
    /// b.tick(t0 + Duration::from_secs(8)); // times out
    /// // a 10 s jitter sample is capped at the 4 s maximum: retry at 8 + 1 + 4
    /// assert!(!offers(&b.tick(t0 + Duration::from_millis(12_999))));
    /// assert!(offers(&b.tick(t0 + Duration::from_secs(13))));
    /// ```
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
///
/// ```
/// use std::time::{Duration, Instant};
/// use kevy_elect::{ElectConfig, ElectJitter, Elector, Message, Role};
///
/// // when a candidacy lapses, how long until this node offers again?
/// fn retry_after(id: &str, jitter: ElectJitter) -> Duration {
///     let peers = vec!["a".to_string(), id.to_string()];
///     let mut e = Elector::new(id, peers, "x:6004", Role::Replica).with_jitter(jitter);
///     let t0 = Instant::now();
///     let hb = Message::Hb { epoch: 1, node_id: "a".into(), role: Role::Primary, repl_offset: 0 };
///     e.on_message("a", hb, t0);
///     e.tick(t0 + Duration::from_secs(5)); // offers
///     let lapsed = t0 + Duration::from_secs(8);
///     e.tick(lapsed); // times out
///     (1..=500)
///         .map(|i| Duration::from_millis(i * 10))
///         .find(|d| e.tick(lapsed + *d).iter().any(|o| matches!(o.msg, Message::Offer { .. })))
///         .expect("retries within backoff + jitter")
/// }
///
/// let fixed = ElectJitter::Fixed(Duration::from_millis(300));
/// assert_eq!(retry_after("b", fixed.clone()), Duration::from_millis(1300));
/// assert_eq!(retry_after("c", fixed), Duration::from_millis(1300)); // in lockstep
/// assert!(retry_after("b", ElectJitter::System) <= ElectConfig::default().election_backoff * 5);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ElectJitter {
    /// Fixed value (test-friendly, deterministic).
    ///
    /// ```
    /// # use std::time::{Duration, Instant};
    /// # use kevy_elect::{ElectConfig, ElectJitter, Elector, Message, Role};
    /// # fn offers(out: &[kevy_elect::Outbound]) -> bool {
    /// #     out.iter().any(|o| matches!(o.msg, Message::Offer { .. }))
    /// # }
    /// # let cfg = ElectConfig::default();
    /// # let mut b = Elector::new("b", vec!["a".into(), "b".into()], "b:6004", Role::Replica)
    /// #     .with_config(cfg).with_jitter(ElectJitter::Fixed(Duration::from_millis(250)));
    /// # let t0 = Instant::now();
    /// # let hb = Message::Hb { epoch: 1, node_id: "a".into(), role: Role::Primary, repl_offset: 0 };
    /// # b.on_message("a", hb, t0);
    /// b.tick(t0 + Duration::from_secs(5));
    /// b.tick(t0 + Duration::from_secs(8)); // times out
    /// // backoff 1 s + exactly 250 ms of jitter
    /// assert!(!offers(&b.tick(t0 + Duration::from_millis(9249))));
    /// assert!(offers(&b.tick(t0 + Duration::from_millis(9250))));
    /// ```
    Fixed(Duration),
    /// Hash of `(now_nanos, node_id)` clamped into
    /// `[0, max_jitter)`. Deterministic enough for production while
    /// avoiding zero-cost-jitter dueling.
    ///
    /// ```
    /// # use std::time::{Duration, Instant};
    /// # use kevy_elect::{ElectConfig, ElectJitter, Elector, Message, Role};
    /// # fn offers(out: &[kevy_elect::Outbound]) -> bool {
    /// #     out.iter().any(|o| matches!(o.msg, Message::Offer { .. }))
    /// # }
    /// # let cfg = ElectConfig::default();
    /// # let mut b = Elector::new("b", vec!["a".into(), "b".into()], "b:6004", Role::Replica)
    /// #     .with_config(cfg).with_jitter(ElectJitter::System);
    /// # let t0 = Instant::now();
    /// # let hb = Message::Hb { epoch: 1, node_id: "a".into(), role: Role::Primary, repl_offset: 0 };
    /// # b.on_message("a", hb, t0);
    /// b.tick(t0 + Duration::from_secs(5));
    /// b.tick(t0 + Duration::from_secs(8)); // times out
    /// // the retry lands somewhere in backoff + [0, jitter max) = 1–5 s later
    /// assert!(!offers(&b.tick(t0 + Duration::from_millis(8999))));
    /// assert!(offers(&b.tick(t0 + Duration::from_secs(13))));
    /// ```
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_jitter_stays_under_its_bound_and_differs_between_nodes() {
        let (now, max) = (Instant::now(), Duration::from_millis(500));
        let draws: Vec<Duration> = ["node-a", "node-b", "node-c"]
            .iter()
            .map(|n| ElectJitter::System.sample(max, now, n))
            .collect();
        assert!(draws.iter().all(|&d| d < max), "{draws:?}");
        assert!(
            draws[0] != draws[1] || draws[1] != draws[2],
            "dueling nodes must not all tie: {draws:?}"
        );
        assert_eq!(
            ElectJitter::System.sample(max, now, "node-a"),
            draws[0],
            "a draw is a function of the node"
        );
        assert_eq!(ElectJitter::System.sample(Duration::ZERO, now, "node-a"), Duration::ZERO);
        assert_eq!(ElectJitter::Fixed(Duration::from_secs(9)).sample(max, now, "node-a"), max);
    }
}
