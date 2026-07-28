//! The transparent-tiering knobs of [`Config`] (child module via
//! `#[path]`, split from `config.rs` for the 500-LOC house rule;
//! behaviour unchanged).

#[cfg(feature = "tier")]
use super::TierBudgetSpec;
use super::Config;

impl Config {
    /// Enable transparent tiering with a RAM budget of `bytes`: values
    /// past the demote watermark spill to the cold tier on disk
    /// (`<data_dir>/tier/`) and page back in on access — every command
    /// keeps its exact semantics. Requires [`Self::with_persist`]; a
    /// memory-only store fails at open with a named error.
    #[cfg(feature = "tier")]
    #[must_use]
    pub fn with_tier_budget(mut self, bytes: u64) -> Self {
        self.tier_budget = Some(TierBudgetSpec::Bytes(bytes));
        self
    }

    /// Enable transparent tiering with the `auto` budget: 0.70 × the
    /// detected memory bound (cgroup v2 limit / `MemAvailable` on
    /// Linux, `hw.memsize` on macOS), re-probed on every reaper tick.
    /// Open fails with a named error when no bound is detectable.
    #[cfg(feature = "tier")]
    #[must_use]
    pub fn with_tier_budget_auto(mut self) -> Self {
        self.tier_budget = Some(TierBudgetSpec::Auto);
        self
    }

    /// Cap the largest spillable value (bytes; 0 = unlimited). Bounds
    /// the embedded cold-read lock-hold time; default 256 KiB.
    #[must_use]
    pub fn with_max_spill_value(mut self, bytes: u64) -> Self {
        self.max_spill_value = bytes;
        self
    }

    /// Enable transparent tiering with a percent-of-detected-bound
    /// budget (`p` in 1..=100 — validated at open with a named error,
    /// like every other config refusal).
    #[cfg(feature = "tier")]
    #[must_use]
    pub fn with_tier_budget_percent(mut self, p: u8) -> Self {
        self.tier_budget = Some(TierBudgetSpec::Percent(p));
        self
    }
}
