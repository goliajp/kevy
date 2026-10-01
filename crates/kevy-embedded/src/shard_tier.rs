//! Tiering budget resolution and the per-tick tiering upkeep, split from
//! `shard.rs` for the 500-LOC house rule.

use std::io;

use crate::config::Config;

/// Resolve the configured tier budget spec to a PER-SHARD byte count
/// (whole-store budget / `nshards`, floored at 1). Named refusals:
/// percent out of 1..=100, auto/percent with no detectable bound.
pub(crate) fn resolve_tier_budget(config: &Config, nshards: usize) -> io::Result<u64> {
    let spec = config
        .tier_budget
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "tiering is not configured"))?;
    resolve_tier_spec(spec, nshards)
}

/// Spec-level half of [`resolve_tier_budget`] — also the reaper tick's
/// re-resolution entry (auto/percent re-probe the memory bound live).
#[cfg(all(feature = "tier", not(target_arch = "wasm32")))]
pub(crate) fn resolve_tier_spec(
    spec: crate::config::TierBudgetSpec,
    nshards: usize,
) -> io::Result<u64> {
    use crate::config::TierBudgetSpec;
    if let TierBudgetSpec::Percent(p) = spec
        && !(1..=100).contains(&p)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("tiering budget percent must be 1..=100, got {p}"),
        ));
    }
    let total = crate::config_tier::resolve(spec).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "tiering budget auto/percent: no memory bound detected on this host — \
             use with_tier_budget(bytes)",
        )
    })?;
    Ok((total / nshards.max(1) as u64).max(1))
}

/// Per-tick tiering upkeep: re-resolve a probe-backed budget and
/// feed the index/view memory floor into the unified watermark. Runs
/// under the shard lock the tick already holds; one branch when
/// tiering is off.
#[cfg(all(feature = "tier", not(target_arch = "wasm32")))]
pub(crate) fn tier_tick_upkeep(
    g: &mut crate::store::Inner,
    spec: Option<crate::config::TierBudgetSpec>,
    nshards: usize,
) {
    use crate::config::TierBudgetSpec;
    if !g.store.tier_enabled() {
        return;
    }
    if let Some(spec @ (TierBudgetSpec::Auto | TierBudgetSpec::Percent(_))) = spec
        && let Ok(per_shard) = resolve_tier_spec(spec, nshards)
    {
        g.store.set_tier_budget(per_shard);
    }
    #[cfg(feature = "index")]
    let reserved = g.idx_segs.reserved_bytes() + g.view_segs.reserved_bytes();
    #[cfg(not(feature = "index"))]
    let reserved = 0u64;
    g.store.set_tier_reserved(reserved);
    g.store.tier_reserve_growth();
}
