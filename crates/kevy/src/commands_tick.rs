//! The shard tick's free helpers (child module via `#[path]`, the
//! house pattern) — split from `commands.rs` at the 500-LOC ceiling.
//! The seam is real: everything here runs on the 100 ms tick, not on
//! a request path.

use kevy_store::Store;

use crate::KevyCommands;

#[inline]
pub(super) fn alloc_reclaim_tick() {
    #[cfg(feature = "kevy-alloc")]
    kevy_alloc::thread_reclaim();
}

/// Re-apply maxmemory + eviction policy in case `CONFIG SET` has
/// swapped the global since the previous tick. `store.set_max_memory`
/// is idempotent and cheap (compares + assigns two scalars + may
/// recompute soft-limit accounting); paying it every 100 ms is well
/// below the noise floor of any benchmark. The instance bound is
/// divided across shards here exactly as at `on_shard_init` —
/// this re-apply used to hand every shard the WHOLE figure, so
/// the init-time division was overwritten within one tick.
pub(super) fn maxmemory_tick(c: &KevyCommands, store: &mut Store, cfg: &kevy_config::Config) {
    let n = c.state().nshards().max(1) as u64;
    store.set_max_memory(
        cfg.memory.maxmemory / n,
        crate::map_eviction_policy(cfg.memory.maxmemory_policy),
    );
}

pub(super) fn tier_tick(c: &KevyCommands, store: &mut Store, bits: u32, cfg: &kevy_config::Config) {
    if !store.tier_enabled() {
        return;
    }
    if let Ok(Some(total)) = crate::resolve_tier_budget(cfg) {
        let n = c.state().nshards().max(1) as u64;
        store.set_tier_budget((total / n).max(1));
    }
    let mut reserved = 0u64;
    if bits & crate::state::IDX_NONEMPTY != 0 {
        reserved += crate::index_runtime::reserved_bytes(&c.ctx(), store);
    }
    if bits & crate::state::VIEW_NONEMPTY != 0 {
        reserved += crate::view_runtime::reserved_bytes(&c.ctx());
    }
    store.set_tier_reserved(reserved);
}
