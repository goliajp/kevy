//! Transparent tiering — the store core.
//!
//! Cold values live in a per-shard [`kevy_vlog::Vlog`]; each leaves a
//! 24-byte [`ColdRef`] stub *in the map* (`Value::Cold`), so every raw
//! probe (SET fast path, DEL, RENAME, FLUSHALL, SCAN's single-table
//! sweep, the reaper) works unchanged by construction. The two-stage
//! funnel keeps the ~196 downstream `Value` matches Cold-free:
//!
//! - **Stage 1 (zero IO)**: existence / NX / XX / EXPIRE-family answer
//!   from the `Entry`; `TYPE` from the stub's tag; a WRONGTYPE refusal
//!   never pays a pread ([`Store::tier_resolve`] / [`Store::tier_serve`]
//!   check the tag before touching disk).
//! - **Stage 2 (materialize)**: write paths promote in place; read
//!   paths run the promotion gate — the FIRST materializing access
//!   serves decoded bytes without installing (probation `touched`
//!   mark), the SECOND promotes. Bulk/`&self` shared-lane reads never
//!   promote and never set the mark.
//!
//! Demotion/promotion are dedicated in-place primitives — NOT
//! `insert_entry`/`remove_entry`, which would clear hash field-TTLs,
//! capture `new` events and drift the `expires` counter. They emit
//! zero keyspace notifications, never bump WATCH versions, and
//! preserve `lru_clock` (LFU history survives a round trip).
//!
//! This module is compiled only with `std` off-wasm (the vlog needs a
//! real filesystem); a sibling `cfg(not(...))` block provides funnel
//! passthroughs so call sites stay cfg-free.

#[cfg(all(feature = "std", not(target_arch = "wasm32")))]
mod enabled {
    use std::io;
    use std::path::{Path, PathBuf};

    use kevy_vlog::{Vlog, VlogRef};

    use crate::value::{ColdRef, Value};
    use crate::{EvictionPolicy, SmallBytes, Store};

    /// Per-shard tiering state — present only when tiering is enabled
    /// (`tier: Option<TierState>`; `None` = today's paths, the A1 gate's
    /// precondition).
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
        /// RAM the cold stubs themselves cost (Σ per cold key of
        /// `ENTRY_OVERHEAD + key heap bytes`) — the other unreclaimable
        /// floor, maintained incrementally at demote / promote /
        /// DEL-of-cold / RENAME / FLUSHALL.
        pub(crate) stub_bytes: u64,
        /// Cold stubs RENAMEd away from their record's embedded key:
        /// `(file_id, offset) → current key`. Rename moves the stub
        /// without a pread, so the on-disk key goes stale; compaction's
        /// `is_live`/`moved` consult this map on a primary-key miss.
        /// Usually empty; entries die with their stub.
        pub(crate) renames: std::collections::HashMap<(u32, u64), SmallBytes>,
    }

    /// Tiering gauges — the `INFO # Tiering` feeders.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct TierStats {
        /// The RAM budget this shard demotes against (resolved bytes).
        pub budget: u64,
        /// The unified demote target: `budget·19/20 − reserved_bytes −
        /// stub_bytes`, saturating. **0 = the floor alone exceeds the
        /// budget** — the tier can demote nothing; visible here, never
        /// silent (RFC §4 row 16).
        pub effective_target: u64,
        /// Index/view memory floor fed by [`Store::set_tier_reserved`].
        pub reserved_bytes: u64,
        /// RAM the cold stubs cost (Σ `ENTRY_OVERHEAD + key heap`).
        pub stub_bytes: u64,
        /// Keys demoted to the cold tier since boot.
        pub demotions_total: u64,
        /// Keys promoted back since boot.
        pub promotions_total: u64,
        /// Vlog record reads (serve + promote + peek).
        pub preads_total: u64,
        /// No-promote peek record reads only — one per cold row.
        pub peek_preads_total: u64,
        /// Batched cold-read submissions — one per page batch on
        /// the sync reader; kernel submit count on the uring reader.
        pub batch_submissions_total: u64,
        /// Currently-cold keys.
        pub cold_keys: u64,
        /// Σ original weights of currently-cold values.
        pub cold_bytes: u64,
        /// Vlog file count.
        pub vlog_files: u64,
        /// Vlog total bytes on disk.
        pub vlog_bytes: u64,
        /// Vlog live (non-dead) bytes.
        pub vlog_live_bytes: u64,
        /// Vlog compaction epoch (retired-file counter).
        pub vlog_epoch: u64,
    }

    impl ColdRef {
        #[inline]
        pub(crate) fn vref(self) -> VlogRef {
            VlogRef { file_id: self.file_id, offset: self.offset, len: self.len }
        }
    }

    impl Store {
        /// Turn tiering on: open (wiping) the vlog under `dir` and set
        /// the RAM `budget` the demotion watermark works against.
        /// Callers own the dir choice (`<data>/tier/` by convention).
        pub fn enable_tiering(&mut self, dir: &Path, budget: u64) -> io::Result<()> {
            let dir: PathBuf = dir.to_path_buf();
            let vlog = Vlog::open(&dir, kevy_vlog::DEFAULT_ROTATE_BYTES)?;
            self.tier = Some(TierState {
                vlog,
                budget,
                policy: EvictionPolicy::AllKeysLru,
                demotions_total: 0,
                tick_wait: 0,
                tick_skip: 0,
                promotions_total: 0,
                preads_total: 0,
                peek_preads_total: 0,
                batch_submissions_total: 0,
                cold_keys: 0,
                cold_bytes: 0,
                max_spill: 0,
                reserved_bytes: 0,
                stub_bytes: 0,
                renames: std::collections::HashMap::new(),
            });
            Ok(())
        }

        /// Live-update the tiering budget (auto/percent re-resolution on
        /// the shard tick, `CONFIG SET` — the maxmemory reapply
        /// precedent). Touches nothing but the number: the vlog, the
        /// stubs and every counter stay as they are. No-op when tiering
        /// is off.
        #[inline]
        pub fn set_tier_budget(&mut self, bytes: u64) {
            if let Some(t) = &mut self.tier {
                t.budget = bytes;
            }
        }

        /// Cap the largest spillable value (0 = unlimited). Embedded
        /// sets 256 KiB by default (RFC §7) to bound cold-read
        /// lock-hold time; the server leaves it unlimited. No-op when
        /// tiering is off.
        #[inline]
        pub fn set_tier_max_spill(&mut self, bytes: u64) {
            if let Some(t) = &mut self.tier {
                t.max_spill = bytes;
            }
        }

        /// Feed the index/view memory floor (Σ segment `approx_bytes`
        /// on this shard) into the unified watermark. Called per shard
        /// tick by the serving layer. No-op when tiering is off.
        #[inline]
        pub fn set_tier_reserved(&mut self, bytes: u64) {
            if let Some(t) = &mut self.tier {
                t.reserved_bytes = bytes;
            }
        }

        /// Whether the index/view floor (`reserved_bytes + extra`)
        /// already exhausts the tier's demotable headroom — the
        /// IDX.CREATE refusal predicate (RFC §4 row 16). `false` when
        /// tiering is off.
        pub fn tier_index_floor_blocked(&self, extra: u64) -> bool {
            match &self.tier {
                Some(t) => {
                    t.reserved_bytes.saturating_add(extra)
                        >= crate::tier_demote::watermark(t.budget).saturating_sub(t.stub_bytes)
                }
                None => false,
            }
        }

        /// Whether tiering is on for this shard.
        #[inline]
        pub fn tier_enabled(&self) -> bool {
            self.tier.is_some()
        }

        /// Tiering gauges — zeros when tiering is off.
        pub fn tier_stats(&self) -> TierStats {
            match &self.tier {
                None => TierStats::default(),
                Some(t) => {
                    let v = t.vlog.stats();
                    TierStats {
                        budget: t.budget,
                        effective_target: crate::tier_demote::effective_target(t),
                        reserved_bytes: t.reserved_bytes,
                        stub_bytes: t.stub_bytes,
                        demotions_total: t.demotions_total,
                        promotions_total: t.promotions_total,
                        preads_total: t.preads_total,
                        peek_preads_total: t.peek_preads_total,
                        batch_submissions_total: t.batch_submissions_total,
                        cold_keys: t.cold_keys,
                        cold_bytes: t.cold_bytes,
                        vlog_files: v.files as u64,
                        vlog_bytes: v.bytes,
                        vlog_live_bytes: v.live_bytes,
                        vlog_epoch: v.epoch,
                    }
                }
            }
        }

        /// Whether the LRU/LFU access clock must advance: eviction
        /// (`maxmemory > 0`) or tiering (demotion scoring) needs it.
        /// Same single-branch cost as the old `maxmemory > 0` test.
        #[inline]
        pub(crate) fn clock_on(&self) -> bool {
            self.maxmemory > 0 || self.tier.is_some()
        }

        /// The policy access-touches score under: eviction's when
        /// enabled, else the tier's (tiered-lru default).
        #[inline]
        pub(crate) fn touch_policy(&self) -> EvictionPolicy {
            if self.maxmemory > 0 {
                return self.eviction_policy;
            }
            match &self.tier {
                Some(t) => t.policy,
                None => self.eviction_policy,
            }
        }

        /// Pin every current vlog file (view pinning): a
        /// snapshot view / rewrite plan captured from a tiered store
        /// carries these so its frozen [`ColdRef`]s stay readable on the
        /// serializer thread across compaction — a retired file is
        /// unlinked only when the last pin drops. Empty when tiering is
        /// off.
        pub fn tier_pins(&self) -> Vec<std::sync::Arc<kevy_vlog::VlogFile>> {
            match &self.tier {
                Some(t) => t.vlog.pin_all(),
                None => Vec::new(),
            }
        }

        /// Serialization-side cold materialization: decode `v`'s
        /// record into a fresh owned hot value WITHOUT installing,
        /// promoting, or setting the probation mark — persistence is a
        /// bulk path and never promotes. `None` when `v` is hot.
        pub fn materialize_cold(&self, key: &[u8], v: &Value) -> Option<Value> {
            self.tier_peek_value(key, v)
        }

        /// A cold stub is being discarded (DEL / overwrite / expiry /
        /// FLUSH of the key): credit its record's bytes as dead so the
        /// compaction trigger sees them, and release the stub's RAM
        /// cost from `stub_bytes`. `key_heap` is the heap-byte cost of
        /// the key the stub lived under (part of the stub cost —
        /// callers pass `key_heap_bytes_for(key)` / `key.heap_bytes()`
        /// since some sites have already moved the key into the map).
        /// No-op for hot values.
        pub(crate) fn tier_note_dead(&mut self, key_heap: u64, v: &Value) {
            let Value::Cold(c) = v else { return };
            if c.is_seg() {
                self.segrow_note_dead(*c);
                return;
            }
            if let Some(t) = &mut self.tier {
                t.vlog.note_dead(c.vref());
                t.cold_keys = t.cold_keys.saturating_sub(1);
                t.cold_bytes = t.cold_bytes.saturating_sub(u64::from(c.weight));
                t.stub_bytes = t
                    .stub_bytes
                    .saturating_sub(crate::value::ENTRY_OVERHEAD + key_heap);
                t.renames.remove(&(c.file_id, c.offset));
            }
        }

        /// RENAME moved a cold stub from `src` to `dst` without reading
        /// it — the record's embedded key is now stale; register the
        /// forward pointer compaction resolves through, and re-account
        /// the stub cost for the new key's heap bytes.
        pub(crate) fn tier_note_renamed(&mut self, v: &Value, src: &[u8], dst: &[u8]) {
            let Value::Cold(c) = v else { return };
            if let Some(t) = &mut self.tier {
                t.stub_bytes = t
                    .stub_bytes
                    .saturating_sub(crate::key_heap_bytes_for(src))
                    .saturating_add(crate::key_heap_bytes_for(dst));
                t.renames.insert((c.file_id, c.offset), SmallBytes::from_slice(dst));
            }
        }

        /// FLUSHALL: every stub died with the map — mark the whole log
        /// dead (sealed files drop scan-free at the next compaction).
        pub(crate) fn tier_on_flushall(&mut self) {
            if let Some(t) = &mut self.tier {
                t.vlog.mark_all_dead();
                t.renames.clear();
                t.cold_keys = 0;
                t.cold_bytes = 0;
                t.stub_bytes = 0;
            }
        }
    }
}

#[cfg(all(feature = "std", not(target_arch = "wasm32")))]
pub use enabled::TierStats;
#[cfg(all(feature = "std", not(target_arch = "wasm32")))]
pub(crate) use enabled::TierState;

/// Funnel passthroughs for builds without the tier backend (no_std /
/// wasm): `Value::Cold` cannot be constructed there (no `enable_tiering`),
/// so the funnel degenerates to `live_entry` and no-ops.
#[cfg(not(all(feature = "std", not(target_arch = "wasm32"))))]
mod disabled {
    use crate::value::Value;
    use crate::{EvictionPolicy, Store};

    impl Store {
        #[inline]
        pub(crate) fn clock_on(&self) -> bool {
            self.maxmemory > 0
        }

        #[inline]
        pub(crate) fn touch_policy(&self) -> EvictionPolicy {
            self.eviction_policy
        }

        /// No tier backend on this target — `Value::Cold` cannot exist.
        #[inline]
        pub fn materialize_cold(&self, _v: &Value) -> Option<Value> {
            None
        }

        /// No tier backend on this target — always 0.
        #[inline]
        pub fn demote_to_watermark(&mut self) -> usize {
            0
        }

        #[inline]
        pub(crate) fn tier_note_dead(&mut self, _key_heap: u64, _v: &Value) {}

        #[inline]
        pub(crate) fn tier_note_renamed(&mut self, _v: &Value, _src: &[u8], _dst: &[u8]) {}

        #[inline]
        pub(crate) fn tier_on_flushall(&mut self) {}

        /// No tier backend on this target — no-op.
        #[inline]
        pub fn set_tier_budget(&mut self, _bytes: u64) {}

        /// No tier backend on this target — no-op.
        #[inline]
        pub fn set_tier_reserved(&mut self, _bytes: u64) {}

        /// No tier backend on this target — always false.
        #[inline]
        pub fn tier_index_floor_blocked(&self, _extra: u64) -> bool {
            false
        }

        #[inline]
        pub(crate) fn promote_in_place(&mut self, _key: &[u8]) -> bool {
            false
        }

        /// No tier backend on this target — always 0.
        #[inline]
        pub fn try_demote_after_write(&mut self) -> usize {
            0
        }

        /// No tier backend on this target — always 0.
        #[inline]
        pub fn demote_step(&mut self) -> usize {
            0
        }

        /// No tier backend on this target — always 0.
        #[inline]
        pub fn tier_compact_tick(&mut self) -> usize {
            0
        }

        /// No tier backend on this target — always false.
        #[doc(hidden)]
        pub fn debug_force_demote(&mut self, _key: &[u8]) -> bool {
            false
        }

        /// No tier backend on this target — always false.
        #[inline]
        pub fn tier_enabled(&self) -> bool {
            false
        }
    }
}
