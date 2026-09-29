//! Auto-rewrite policy — WHEN the AOF should compact itself. Split from
//! `aof.rs` for the 500-LOC house rule; the mechanics (rewrite_from /
//! concurrent rewrite) stay there, this file owns only the decision.

use crate::aof::Aof;

/// When to auto-compact the AOF. Three independent triggers — ANY hit fires
/// a rewrite (each disabled by its zero): growth (`pct` percent past the
/// last-rewrite size, gated by `min_size` — Redis's classic pair), an
/// absolute cap (`bytes`), and staleness (`interval_secs` since the last
/// rewrite, provided the log actually grew). The growth rule alone is too
/// sluggish for long-lived instances: a 2.2 GB log must reach 4.4 GB before
/// 100% growth fires, and a real deployment rode that to 12-second replays
/// and an OOM loop — the absolute and time rules exist to cap exactly that.
///
/// The default has every rule off: the log is never compacted on its own.
///
/// ```
/// use kevy_persist::RewritePolicy;
///
/// let p = RewritePolicy::default().with_pct(100).with_min_size(64 << 20);
/// // a 1 MiB log reaches 64 MiB only by growing past any baseline
/// assert!(!p.baseline_matters(1 << 20));
/// assert!(p.baseline_matters(48 << 20));
/// assert!(p.with_interval_secs(3600).baseline_matters(0));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub struct RewritePolicy {
    /// Growth percentage past the last-rewrite baseline (0 = rule off).
    pub pct: u32,
    /// Minimum size before the growth rule may fire.
    pub min_size: u64,
    /// Absolute size cap (0 = rule off).
    pub bytes: u64,
    /// Rewrite at least this often while the log grows (0 = rule off).
    pub interval_secs: u64,
}

impl RewritePolicy {
    /// Set [`RewritePolicy::pct`].
    ///
    /// ```
    /// assert_eq!(kevy_persist::RewritePolicy::default().with_pct(50).pct, 50);
    /// ```
    #[must_use]
    pub fn with_pct(mut self, pct: u32) -> Self {
        self.pct = pct;
        self
    }

    /// Set [`RewritePolicy::min_size`].
    ///
    /// ```
    /// assert_eq!(kevy_persist::RewritePolicy::default().with_min_size(1 << 20).min_size, 1 << 20);
    /// ```
    #[must_use]
    pub fn with_min_size(mut self, bytes: u64) -> Self {
        self.min_size = bytes;
        self
    }

    /// Set [`RewritePolicy::bytes`].
    ///
    /// ```
    /// assert_eq!(kevy_persist::RewritePolicy::default().with_bytes(1 << 30).bytes, 1 << 30);
    /// ```
    #[must_use]
    pub fn with_bytes(mut self, bytes: u64) -> Self {
        self.bytes = bytes;
        self
    }

    /// Set [`RewritePolicy::interval_secs`].
    ///
    /// ```
    /// assert_eq!(kevy_persist::RewritePolicy::default().with_interval_secs(60).interval_secs, 60);
    /// ```
    #[must_use]
    pub fn with_interval_secs(mut self, secs: u64) -> Self {
        self.interval_secs = secs;
        self
    }

    /// Whether the growth-rule baseline can change any decision for a log
    /// that is `len` bytes long at open. It cannot when the staleness rule
    /// is off and `len` is so far under `min_size` that growth past the
    /// baseline is already implied by reaching `min_size`: the rule then
    /// fires at `min_size` whatever the baseline, provided the baseline is
    /// at most `len`. Open paths skip the O(keys) baseline estimate then.
    pub fn baseline_matters(&self, len: u64) -> bool {
        self.interval_secs > 0
            || (self.pct > 0
                && len.saturating_mul(100u64.saturating_add(u64::from(self.pct)))
                    > self.min_size.saturating_mul(100))
    }
}

impl Aof {
    /// Should this AOF be auto-compacted under `policy`? See
    /// [`RewritePolicy`] for the three rules. Always false while a
    /// concurrent rewrite is already in flight.
    pub fn rewrite_due(&self, policy: RewritePolicy) -> bool {
        if self.is_rewriting() {
            return false;
        }
        let cur = self.size_bytes();
        let baseline = self.size_at_last_rewrite().max(1);
        // Growth: (cur - baseline)/baseline >= pct%, once past min_size.
        if policy.pct > 0
            && cur >= policy.min_size
            && cur.saturating_mul(100)
                >= baseline.saturating_mul(100u64.saturating_add(u64::from(policy.pct)))
        {
            return true;
        }
        // Absolute cap: the log is simply too big, growth ratio be damned.
        if policy.bytes > 0 && cur >= policy.bytes {
            return true;
        }
        // Staleness: it has been a while AND there is something to fold in.
        policy.interval_secs > 0
            && cur > self.size_at_last_rewrite()
            && self.last_rewrite_at().elapsed().as_secs() >= policy.interval_secs
    }
}
