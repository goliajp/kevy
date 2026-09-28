//! The persistence knobs that are derived rather than set one by one.
//! Split from `config.rs` to keep it under the 500-line cap.

use crate::config::Config;

impl Config {
    /// Stage appends in a ring of `bytes` per shard (a power of two, at
    /// least 64 KiB), or turn staging off with 0. See
    /// [`Config::stage_bytes`].
    ///
    /// ```
    /// let config = kevy_embedded::Config::default().with_stage_ring(1 << 20);
    /// assert_eq!(config.stage_bytes, 1 << 20);
    /// ```
    pub fn with_stage_ring(mut self, bytes: u64) -> Self {
        assert!(
            bytes == 0 || (bytes.is_power_of_two() && bytes >= 64 * 1024),
            "a staging ring is 0 or a power of two of at least 64 KiB, not {bytes}"
        );
        self.stage_bytes = bytes;
        self
    }

    /// The auto-rewrite rules this config asks for.
    #[cfg(feature = "persist")]
    pub(crate) fn rewrite_policy(&self) -> kevy_persist::RewritePolicy {
        kevy_persist::RewritePolicy {
            pct: self.auto_aof_rewrite_pct,
            min_size: self.auto_aof_rewrite_min_size,
            bytes: self.auto_aof_rewrite_bytes,
            interval_secs: self.auto_aof_rewrite_interval_secs,
        }
    }
}
