//! The tiering budget spec: kevy-config's, the same `[tiering] budget`
//! forms the server reads. Resolution happens at open and re-runs on
//! every reaper tick for the probe-backed forms.

pub use kevy_config::TierBudgetSpec;

/// Resolve to bytes against the OS memory bound, probed through
/// `kevy-sys` (the sanctioned boundary). `None` = the probe found no
/// bound (the open path turns that into a named error, never a silent
/// off). On wasm32 there is no OS to ask, and kevy-sys never links there.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn resolve(spec: TierBudgetSpec) -> Option<u64> {
    spec.resolve_with(kevy_sys::detected_memory_bound())
}
