//! Transparent tiering — the store core (capacity arc T3, RFC
//! 2026-07-24-v5-capacity-arc §1 D0/D2).
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
    use crate::{Entry, EvictionPolicy, SmallBytes, Store, StoreError};

    /// Per-shard tiering state — present only when tiering is enabled
    /// (`tier: Option<TierState>`; `None` = today's paths, the A1 gate's
    /// precondition).
    pub(crate) struct TierState {
        pub(crate) vlog: Vlog,
        pub(crate) budget: u64,
        /// Demotion victim scoring (RFC §7: tiered-lru default).
        pub(crate) policy: EvictionPolicy,
        pub(crate) demotions_total: u64,
        pub(crate) promotions_total: u64,
        /// Every vlog record read (serve, promote, peek) — the
        /// WRONGTYPE-without-read proof counter.
        pub(crate) preads_total: u64,
        pub(crate) cold_keys: u64,
        pub(crate) cold_bytes: u64,
        /// Cold stubs RENAMEd away from their record's embedded key:
        /// `(file_id, offset) → current key`. Rename moves the stub
        /// without a pread, so the on-disk key goes stale; compaction's
        /// `is_live`/`moved` consult this map on a primary-key miss.
        /// Usually empty; entries die with their stub.
        pub(crate) renames: std::collections::HashMap<(u32, u64), SmallBytes>,
    }

    /// Tiering gauges (`INFO` feeders land in T5; accessors now for B12).
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct TierStats {
        /// Keys demoted to the cold tier since boot.
        pub demotions_total: u64,
        /// Keys promoted back since boot.
        pub promotions_total: u64,
        /// Vlog record reads (serve + promote + peek).
        pub preads_total: u64,
        /// Currently-cold keys.
        pub cold_keys: u64,
        /// Σ original weights of currently-cold values.
        pub cold_bytes: u64,
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
                promotions_total: 0,
                preads_total: 0,
                cold_keys: 0,
                cold_bytes: 0,
                renames: std::collections::HashMap::new(),
            });
            Ok(())
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
                Some(t) => TierStats {
                    demotions_total: t.demotions_total,
                    promotions_total: t.promotions_total,
                    preads_total: t.preads_total,
                    cold_keys: t.cold_keys,
                    cold_bytes: t.cold_bytes,
                },
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

        /// Stage-2 funnel for WRITE paths: a live Cold entry whose tag
        /// matches `want` is promoted in place; a mismatch is WRONGTYPE
        /// with zero preads. Hot values / absent keys pass through.
        pub(crate) fn tier_resolve(&mut self, key: &[u8], want: u8) -> Result<(), StoreError> {
            if self.tier.is_none() {
                return Ok(());
            }
            let tag = match self.live_entry(key) {
                Some(Entry { value: Value::Cold(c), .. }) => c.type_tag,
                _ => return Ok(()),
            };
            if tag != want {
                return Err(StoreError::WrongType);
            }
            self.promote_in_place(key);
            Ok(())
        }

        /// Stage-2 funnel for READ paths — `live_entry` plus the
        /// promotion gate. On a live Cold entry with a matching tag:
        /// first materializing access decodes into the serve scratch
        /// (no install, probation mark set); the second promotes. A tag
        /// mismatch is WRONGTYPE with zero preads. Hot/absent =
        /// `live_entry` verbatim.
        pub(crate) fn tier_serve(&mut self, key: &[u8], want: u8) -> Result<Option<&Entry>, StoreError> {
            if self.tier.is_none() {
                return Ok(self.live_entry(key));
            }
            let cold = match self.live_entry(key) {
                None => return Ok(None),
                Some(e) => match &e.value {
                    Value::Cold(c) => Some((c.type_tag, c.touched != 0)),
                    _ => None,
                },
            };
            match cold {
                None => Ok(self.live_entry(key)),
                Some((tag, _)) if tag != want => Err(StoreError::WrongType),
                Some((_, true)) => {
                    self.promote_in_place(key);
                    Ok(self.live_entry(key))
                }
                Some((_, false)) => self.tier_serve_cold(key),
            }
        }

        /// First-touch cold serve: decode the record into the scratch
        /// entry (probation mark set, nothing installed) and hand a
        /// reference to it. The scratch mirrors the live entry's TTL so
        /// callers that read `expire_at_ns` behave identically.
        fn tier_serve_cold(&mut self, key: &[u8]) -> Result<Option<&Entry>, StoreError> {
            let (cref, expire) = {
                let e = self.map.get_mut(key).expect("probed live above");
                let Value::Cold(c) = &mut e.value else { unreachable!("cold checked above") };
                c.touched = 1;
                (*c, e.expire_at_ns)
            };
            let value = self.tier_read_record(cref);
            let mut entry = Entry::new(value, None);
            entry.expire_at_ns = expire;
            entry.set_weight(u64::from(cref.weight));
            self.tier_scratch = Some(entry);
            Ok(self.tier_scratch.as_ref())
        }

        /// Read + decode one cold record (bumps the pread counter). A
        /// vlog read/decode failure is a process bug by the vlog's
        /// per-boot doctrine — surfaced loudly, never healed silently.
        pub(crate) fn tier_read_record(&mut self, cref: ColdRef) -> Value {
            let t = self.tier.as_mut().expect("tier enabled");
            t.preads_total += 1;
            let (_key, payload) = t
                .vlog
                .read(cref.vref())
                .expect("tier: vlog read failed — per-boot spill file, this is a process bug");
            crate::tier_codec::decode(cref.type_tag, payload)
                .expect("tier: cold record decode failed — process bug")
        }

        /// `&self` peek for the zero-copy shared lane and COPY: decode
        /// a fresh owned value from the record WITHOUT installing,
        /// promoting, or setting the probation mark (documented: the
        /// shared lane pays a pread until a `&mut`-path access
        /// promotes). `None` when the value is not Cold.
        pub(crate) fn tier_peek_value(&self, v: &Value) -> Option<Value> {
            let Value::Cold(c) = v else { return None };
            let t = self.tier.as_ref().expect("cold value ⇒ tiering on");
            let (_key, payload) = t
                .vlog
                .read(c.vref())
                .expect("tier: vlog read failed — per-boot spill file, this is a process bug");
            Some(
                crate::tier_codec::decode(c.type_tag, payload)
                    .expect("tier: cold record decode failed — process bug"),
            )
        }

        /// A cold stub is being discarded (DEL / overwrite / expiry /
        /// FLUSH of the key): credit its record's bytes as dead so the
        /// compaction trigger sees them. No-op for hot values.
        pub(crate) fn tier_note_dead(&mut self, v: &Value) {
            let Value::Cold(c) = v else { return };
            if let Some(t) = &mut self.tier {
                t.vlog.note_dead(c.vref());
                t.cold_keys = t.cold_keys.saturating_sub(1);
                t.cold_bytes = t.cold_bytes.saturating_sub(u64::from(c.weight));
                t.renames.remove(&(c.file_id, c.offset));
            }
        }

        /// RENAME moved a cold stub to `dst` without reading it — the
        /// record's embedded key is now stale; register the forward
        /// pointer compaction resolves through.
        pub(crate) fn tier_note_renamed(&mut self, v: &Value, dst: &[u8]) {
            let Value::Cold(c) = v else { return };
            if let Some(t) = &mut self.tier {
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
    use crate::{Entry, EvictionPolicy, Store, StoreError};

    impl Store {
        #[inline]
        pub(crate) fn clock_on(&self) -> bool {
            self.maxmemory > 0
        }

        #[inline]
        pub(crate) fn touch_policy(&self) -> EvictionPolicy {
            self.eviction_policy
        }

        #[inline]
        pub(crate) fn tier_resolve(&mut self, _key: &[u8], _want: u8) -> Result<(), StoreError> {
            Ok(())
        }

        #[inline]
        pub(crate) fn tier_serve(&mut self, key: &[u8], _want: u8) -> Result<Option<&Entry>, StoreError> {
            Ok(self.live_entry(key))
        }

        #[inline]
        pub(crate) fn tier_peek_value(&self, _v: &Value) -> Option<Value> {
            None
        }

        #[inline]
        pub(crate) fn tier_note_dead(&mut self, _v: &Value) {}

        #[inline]
        pub(crate) fn tier_note_renamed(&mut self, _v: &Value, _dst: &[u8]) {}

        #[inline]
        pub(crate) fn tier_on_flushall(&mut self) {}

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
