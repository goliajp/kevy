//! The tiering budget spec — split out of
//! [`crate::config`] for the 500-LOC house rule. Mirrors the server's
//! `[tiering] budget` forms; resolution happens at open and re-runs on
//! every reaper tick for the probe-backed forms.

/// One transparent-tiering RAM budget, as configured. Resolution to
/// bytes probes the OS memory bound through `kevy-sys` (the sanctioned
/// boundary).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TierBudgetSpec {
    /// 0.70 × the detected memory bound (cgroup v2 `memory.max` /
    /// `/proc/meminfo MemAvailable` on Linux; `hw.memsize` on macOS).
    Auto,
    /// That percent of the detected bound (validated 1..=100 at open).
    Percent(u8),
    /// Absolute bytes.
    Bytes(u64),
}

impl TierBudgetSpec {
    /// The `auto` form's fraction of the detected bound (RFC §7: 0.70).
    const AUTO_PCT: u64 = 70;

    /// Resolve to bytes. `None` = the probe found no bound (the open
    /// path turns that into a named error, never a silent off).
    pub(crate) fn resolve(self) -> Option<u64> {
        match self {
            Self::Bytes(b) => Some(b),
            Self::Auto => kevy_sys::detected_memory_bound().map(|d| d / 100 * Self::AUTO_PCT),
            Self::Percent(p) => {
                kevy_sys::detected_memory_bound().map(|d| d / 100 * u64::from(p))
            }
        }
    }
}
