//! Introspection on a live [`Store`] — the embedded-mode answer to Redis's
//! `INFO` / `DBSIZE` / `TTL` / expire-set diagnostics. In-process mode has no
//! TCP endpoint to point `redis-cli` at, so these expose the same signals as
//! plain method calls on the `Store` handle.

use std::time::Duration;

use crate::store::Store;

/// Snapshot of a store's runtime counters, returned by [`Store::info`]. A
/// cheap aggregate (one mutex lock); fields mirror the individual accessors.
///
/// ```
/// let s = kevy_embedded::Store::open(kevy_embedded::Config::default())?;
/// s.set(b"k", b"v")?;
/// let info = s.info();
/// assert_eq!(info.keys, 1);
/// assert!(info.tiering.is_none());
/// # Ok::<(), kevy_embedded::KevyError>(())
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct KevyInfo {
    /// Live key count (`DBSIZE`).
    pub keys: usize,
    /// Estimated resident bytes (`INFO memory: used_memory`).
    pub used_memory: u64,
    /// Current on-disk AOF size in bytes (0 when persistence is off).
    pub aof_bytes: u64,
    /// Live keys carrying a TTL — the expire-set size. A `0` here when you
    /// expected TTLs is the tell that the TTL subsystem didn't register them.
    pub expire_pending: usize,
    /// Total keys evicted by `maxmemory` so far.
    pub evictions: u64,
    /// Total keys expired (lazy + active reaper) so far.
    pub expired_keys: u64,
    /// Tiering gauges (the `# Tiering` INFO section). `None` when
    /// tiering is off — the untiered snapshot is unchanged.
    pub tiering: Option<KevyTierInfo>,
}

/// The `# Tiering` gauge set (B12) — summed across shards; field names
/// mirror the server's `INFO # Tiering` section one-to-one.
///
/// ```
/// let s = kevy_embedded::Store::open(kevy_embedded::Config::default())?;
/// assert!(s.info().tiering.is_none()); // tiering off: no gauges
/// # Ok::<(), kevy_embedded::KevyError>(())
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct KevyTierInfo {
    /// The resolved RAM budget (whole store — Σ per-shard slices).
    pub tier_budget_bytes: u64,
    /// The unified demote target (`budget·19/20 − index floor − stub
    /// floor`, saturating). **0 = the floor alone exceeds the budget.**
    pub tier_effective_target: u64,
    /// Currently-cold keys.
    pub cold_keys: u64,
    /// Σ original weights of currently-cold values.
    pub cold_bytes: u64,
    /// RAM the cold stubs cost (Σ `ENTRY_OVERHEAD + key heap bytes`).
    pub stub_bytes: u64,
    /// The index/view memory floor fed on the reaper tick.
    pub index_reserved_bytes: u64,
    /// Vlog bytes on disk.
    pub vlog_size_bytes: u64,
    /// Vlog live (non-dead) bytes.
    pub vlog_live_bytes: u64,
    /// Vlog file count.
    pub vlog_files: u64,
    /// Vlog compaction epoch (retired files).
    pub vlog_epoch: u64,
    /// Keys demoted since boot.
    pub demotions_total: u64,
    /// Keys promoted back since boot.
    pub promotions_total: u64,
    /// No-promote peek record reads — one per cold row swept by
    /// hydration / backfill / digest / export.
    pub peek_preads_total: u64,
    /// Batched cold-read submissions — one per peeked page.
    pub batch_submissions_total: u64,
}

/// The value log's compression accounting, summed across shards; field
/// names mirror the last four lines of the server's `INFO # Tiering`.
/// Payload plus frame headers against raw bytes is the ratio; the
/// dictionaries are memory, one per vlog file.
/// # Examples
///
/// ```
/// use kevy_embedded::{Config, Store};
/// // untiered: the section does not exist, so neither do its terms
/// let s = Store::open(Config::default()).unwrap();
/// assert!(s.tier_compression().is_none());
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct KevyTierCompression {
    /// Value bytes before encoding, over the vlog files on disk.
    pub vlog_raw_bytes: u64,
    /// Encoded payload bytes on disk, frame headers excluded.
    pub vlog_payload_bytes: u64,
    /// Frame header bytes on disk (tag + LEB128 length per record).
    pub vlog_frame_header_bytes: u64,
    /// Dictionary bytes in memory.
    pub vlog_dict_bytes: u64,
}

impl Store {
    /// One-shot snapshot of the store's introspection counters. See
    /// [`KevyInfo`]. Takes the embedded mutex once; safe to call from a
    /// health endpoint.
    pub fn info(&self) -> KevyInfo {
        KevyInfo {
            keys: self.sum_shards(|i| i.store.dbsize()),
            used_memory: self.sum_shards_u64(|i| i.store.used_memory()),
            #[cfg(feature = "persist")]
            aof_bytes: self
                .sum_shards_u64(|i| i.aof.as_ref().map_or(0, kevy_persist::Aof::size_bytes)),
            #[cfg(not(feature = "persist"))]
            aof_bytes: 0,
            expire_pending: self.sum_shards(|i| i.store.ttl_pending_count()),
            evictions: self.sum_shards_u64(|i| i.store.evictions_total()),
            expired_keys: self.sum_shards_u64(|i| i.store.expired_keys_total()),
            tiering: self.tier_info(),
        }
    }

    /// The `# Tiering` gauges summed across shards, or `None` when
    /// tiering is off (the section is absent, not zeroed — INFO
    /// stability for untiered stores).
    #[cfg(all(feature = "tier", not(target_arch = "wasm32")))]
    pub fn tier_info(&self) -> Option<KevyTierInfo> {
        self.config.tier_budget?;
        let mut t = KevyTierInfo::default();
        for shard in self.shards.iter() {
            let g = crate::store::lock_read(shard);
            let s = g.store.tier_stats();
            t.tier_budget_bytes += s.budget;
            t.tier_effective_target += s.effective_target;
            t.cold_keys += s.cold_keys;
            t.cold_bytes += s.cold_bytes;
            t.stub_bytes += s.stub_bytes;
            t.index_reserved_bytes += s.reserved_bytes;
            t.vlog_size_bytes += s.vlog_bytes;
            t.vlog_live_bytes += s.vlog_live_bytes;
            t.vlog_files += s.vlog_files;
            t.vlog_epoch += s.vlog_epoch;
            t.demotions_total += s.demotions_total;
            t.promotions_total += s.promotions_total;
            t.peek_preads_total += s.peek_preads_total;
            t.batch_submissions_total += s.batch_submissions_total;
        }
        Some(t)
    }

    /// No tier backend compiled in — never a section.
    #[cfg(not(all(feature = "tier", not(target_arch = "wasm32"))))]
    pub fn tier_info(&self) -> Option<KevyTierInfo> {
        None
    }

    /// The value log's compression accounting summed across shards, or
    /// `None` when tiering is off. See [`KevyTierCompression`].
    /// # Examples
    ///
    /// ```
    /// use kevy_embedded::{Config, Store};
    /// let dir = kevy_tmpdir::TmpDir::new("tier-compression-doc");
    /// let cfg = Config::default().with_persist(dir.path()).with_tier_budget(1 << 20);
    /// let s = Store::open(cfg).unwrap();
    /// s.set(b"cold", &[b'x'; 4096]).unwrap();
    /// assert!(s.debug_force_demote(b"cold"));
    /// let c = s.tier_compression().unwrap();
    /// assert_eq!(c.vlog_raw_bytes, 4096);
    /// // a run of one byte keeps a small fraction of itself
    /// assert!(c.vlog_payload_bytes < 4096);
    /// ```
    #[cfg(all(feature = "tier", not(target_arch = "wasm32")))]
    pub fn tier_compression(&self) -> Option<KevyTierCompression> {
        self.config.tier_budget?;
        let mut t = KevyTierCompression::default();
        for shard in self.shards.iter() {
            let c = crate::store::lock_read(shard).store.tier_compression();
            t.vlog_raw_bytes += c.raw_bytes;
            t.vlog_payload_bytes += c.payload_bytes;
            t.vlog_frame_header_bytes += c.frame_header_bytes;
            t.vlog_dict_bytes += c.dict_bytes;
        }
        Some(t)
    }

    /// No tier backend compiled in — never a section.
    #[cfg(not(all(feature = "tier", not(target_arch = "wasm32"))))]
    pub fn tier_compression(&self) -> Option<KevyTierCompression> {
        None
    }

    /// Number of live keys that currently carry a TTL (the expire-set size,
    /// summed across shards).
    pub fn expire_pending_count(&self) -> usize {
        self.sum_shards(|i| i.store.ttl_pending_count())
    }

    /// Remaining TTL for `key` as a [`Duration`], or `None` when the key is
    /// absent or has no TTL (persistent). For the raw Redis `PTTL` sentinels
    /// (`-2` no key, `-1` no TTL) use [`Store::ttl_ms`].
    pub fn ttl(&self, key: &[u8]) -> Option<Duration> {
        let ms = self.wshard(key).store.pttl(key);
        if ms < 0 { None } else { Some(Duration::from_millis(ms as u64)) }
    }
}
