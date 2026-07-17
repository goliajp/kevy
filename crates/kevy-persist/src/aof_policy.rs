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
#[derive(Debug, Clone, Copy)]
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
