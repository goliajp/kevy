//! The shard tick's free helpers (child module via `#[path]`, the
//! house pattern) — split from `commands.rs` at the 500-LOC ceiling.
//! The seam is real: everything here runs on the 100 ms tick, not on
//! a request path.

use kevy_store::Store;

use crate::KevyCommands;
use kevy_rt::Commands as _;

/// Hand back free pages this shard's allocator holds. Returning pages
/// is the one thing kevy-alloc does that glibc's brk arena cannot, and
/// it does nothing until something asks: an allocator has no tick of its
/// own. Measured with it unwired, the resident ratio was 2.39x against
/// glibc's 2.40x — the design's whole point, absent.
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

/// The shard tick's tiering upkeep: re-resolve the
/// budget spec — auto/percent re-probe the cgroup/meminfo bound so
/// live limit changes are honored (the maxmemory reapply precedent) —
/// and feed the index/view memory floor into the unified watermark.
/// Gated on tiering being on: an untiered tick pays one branch.
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

/// Sweep due hash-field TTLs, and announce what the sweep removed.
///
/// Deadlines live in the AOF (`HPEXPIREAT` frames), so replay purges
/// identically — no logging needed here, the same determinism argument as
/// key TTLs.
///
/// The reaper already returns the keys whose fields it dropped. Throwing
/// that away left every structure derived from those rows believing the
/// field was still there: a covering `VALUES` copy outlived the field it
/// copies, so `FILTER` went on selecting rows by a value `HGET` already
/// answered nil for. A field expiring is not a write and reaches no hook on
/// its own — announcing it here is what makes it one.
pub(super) fn sweep_hash_field_ttls(cmds: &KevyCommands, store: &mut Store) {
    for (key, _fields) in store.tick_hash_ttl(64) {
        cmds.on_write(store, &key);
    }
}
