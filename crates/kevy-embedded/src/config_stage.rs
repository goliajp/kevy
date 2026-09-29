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

    /// Append through a mapping of the AOF (`true`) or through `write()`.
    /// See [`Config::mapped_aof`].
    ///
    /// ```
    /// let config = kevy_embedded::Config::default().with_mapped_aof(false);
    /// assert!(!config.mapped_aof);
    /// ```
    pub fn with_mapped_aof(mut self, on: bool) -> Self {
        self.mapped_aof = on;
        self
    }

    /// The auto-rewrite rules this config asks for.
    #[cfg(feature = "persist")]
    pub(crate) fn rewrite_policy(&self) -> kevy_persist::RewritePolicy {
        kevy_persist::RewritePolicy::default()
            .with_pct(self.auto_aof_rewrite_pct)
            .with_min_size(self.auto_aof_rewrite_min_size)
            .with_bytes(self.auto_aof_rewrite_bytes)
            .with_interval_secs(self.auto_aof_rewrite_interval_secs)
    }

    /// What replay and the AOF open do at a corrupt record.
    #[cfg(feature = "persist")]
    pub(crate) fn replay_mode(&self) -> kevy_persist::ReplayMode {
        if self.replay_resync {
            kevy_persist::ReplayMode::Resync
        } else {
            kevy_persist::ReplayMode::Strict
        }
    }
}
