//! The per-shard tiering state (tier backend builds only).

use kevy_vlog::Vlog;

use crate::{EvictionPolicy, SmallBytes};

/// Per-shard tiering state — present only when tiering is enabled
/// (`tier: Option<TierState>`; `None` = today's paths, the A1 gate's
/// precondition).
#[derive(Debug)]
pub(crate) struct TierState {
    pub(crate) vlog: Vlog,
    pub(crate) budget: u64,
    /// Demotion victim scoring (RFC §7: tiered-lru default).
    pub(crate) policy: EvictionPolicy,
    pub(crate) demotions_total: u64,
    /// Demote-sampler backoff: ticks left to skip before
    /// the next over-target sample walk. "Idempotent is not
    /// convergent" — a store that is over target with nothing left
    /// to spill (every spillable value already cold, or the floor
    /// alone exceeds the budget so `effective_target == 0`) used to
    /// re-walk the sample window every tick forever.
    pub(crate) tick_wait: u32,
    /// Current backoff width: doubles on every dry tick batch up
    /// to [`crate::tier_demote::BACKOFF_CEILING_TICKS`], resets to
    /// 0 on any demotion (tick or write path — the write path
    /// always samples immediately, so a fresh spillable value
    /// never waits out the window).
    pub(crate) tick_skip: u32,
    /// Where the demotion sampler starts its next window. It moves
    /// past each window it walks, so the sampler sweeps the table
    /// with or without accesses: a start drawn from the access clock
    /// stood still while nothing but a backfill ran, and demotion
    /// stalled once that one window had gone cold.
    pub(crate) hand: usize,
    pub(crate) promotions_total: u64,
    /// Every vlog record read (serve, promote, peek) — the
    /// WRONGTYPE-without-read proof counter.
    pub(crate) preads_total: u64,
    /// Record reads made by NO-PROMOTE peeks only: hydration,
    /// backfill, digest, scope-move. One per cold ROW — the
    /// preads==rows (not rows×fields) proof counter.
    pub(crate) peek_preads_total: u64,
    /// Batched cold-read submissions: one per
    /// [`Store::peek_hash_rows`] page with ≥1 cold row, weighted by
    /// the reader's kernel submission count — the one-batch-per-page
    /// proof counter.
    pub(crate) batch_submissions_total: u64,
    pub(crate) cold_keys: u64,
    pub(crate) cold_bytes: u64,
    /// Largest value weight demotion may spill (bytes; 0 =
    /// unlimited). Bounds the pread-under-shard-lock hold time on
    /// the embedded RwLock shape (RFC §7: embedded default 256 KiB,
    /// server unlimited) — an over-cap value simply stays hot.
    pub(crate) max_spill: u64,
    /// Index/view memory floor (Σ segment `approx_bytes` on this
    /// shard), fed per shard tick by [`Store::set_tier_reserved`].
    /// Subtracted from the demote watermark: the
    /// premium fixed layer demotion can never reclaim.
    pub(crate) reserved_bytes: u64,
    /// This shard's share of what the process holds live outside
    /// `used_memory` and the index floor (buffers, rings, allocator
    /// overhead), measured by the serving layer. Lowers the target
    /// like the index floor does.
    pub(crate) overhead_bytes: u64,
    /// What the keyspace table's next growth will add, set aside while
    /// the table is within an eighth of it.
    pub(crate) growth_reserve: u64,
    /// RAM the cold stubs themselves cost (Σ per cold key of
    /// `ENTRY_OVERHEAD + key heap bytes`), maintained incrementally
    /// at demote / promote / DEL-of-cold / RENAME / FLUSHALL. A gauge:
    /// the stubs are already inside `used_memory`, so the demote
    /// target does not subtract it.
    pub(crate) stub_bytes: u64,
    /// Cold stubs RENAMEd away from their record's embedded key:
    /// `(file_id, offset) → current key`. Rename moves the stub
    /// without a pread, so the on-disk key goes stale; compaction's
    /// `is_live`/`moved` consult this map on a primary-key miss.
    /// Usually empty; entries die with their stub.
    pub(crate) renames: std::collections::HashMap<(u32, u64), SmallBytes>,
}
