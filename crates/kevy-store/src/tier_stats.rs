//! [`TierStats`], the tiering gauges. Split from `tier.rs` for the
//! 500-LOC house rule.

/// Tiering gauges — the `INFO # Tiering` feeders.
///
/// ```
/// use kevy_store::{SetCondition, Store, TierStats};
/// # let dir = std::env::temp_dir().join(format!("kevy-doc-tierstats-{}", std::process::id()));
/// let mut s = Store::new();
/// assert_eq!(s.tier_stats(), TierStats::default(), "all zero while tiering is off");
/// s.enable_tiering(&dir, 1 << 20)?;
/// s.set(b"k", vec![b'x'; 4096], None, SetCondition::Always);
/// s.set_tier_budget(1);
/// assert_eq!(s.demote_to_watermark(), 1);
/// assert_eq!(s.tier_stats().cold_keys, 1);
/// # std::fs::remove_dir_all(&dir)?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct TierStats {
    /// The RAM budget this shard demotes against (resolved bytes).
    ///
    /// ```
    /// use kevy_store::Store;
    /// # let dir = std::env::temp_dir().join(format!("kevy-doc-budget-{}", std::process::id()));
    /// let mut s = Store::new();
    /// s.enable_tiering(&dir, 1 << 20)?;
    /// assert_eq!(s.tier_stats().budget, 1 << 20);
    /// s.set_tier_budget(4096);
    /// assert_eq!(s.tier_stats().budget, 4096);
    /// # std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub budget: u64,
    /// The unified demote target `used_memory` is held to: `budget·19/20
    /// − reserved_bytes`, saturating. Cold stubs are inside `used_memory`
    /// already, so they do not lower it. **0 = the index floor alone
    /// exceeds the budget** — the tier can demote nothing; visible here,
    /// never silent.
    ///
    /// ```
    /// use kevy_store::Store;
    /// # let dir = std::env::temp_dir().join(format!("kevy-doc-target-{}", std::process::id()));
    /// let mut s = Store::new();
    /// s.enable_tiering(&dir, 1 << 20)?;
    /// assert_eq!(s.tier_stats().effective_target, (1 << 20) * 19 / 20);
    /// s.set_tier_reserved(2 << 20);
    /// assert_eq!(s.tier_stats().effective_target, 0, "the floor exceeds the budget");
    /// # std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub effective_target: u64,
    /// Index/view memory floor fed by [`Store::set_tier_reserved`](crate::Store::set_tier_reserved).
    ///
    /// ```
    /// use kevy_store::Store;
    /// # let dir = std::env::temp_dir().join(format!("kevy-doc-reserved-{}", std::process::id()));
    /// let mut s = Store::new();
    /// s.enable_tiering(&dir, 1 << 20)?;
    /// s.set_tier_reserved(100);
    /// let st = s.tier_stats();
    /// assert_eq!(st.reserved_bytes, 100);
    /// assert_eq!(st.effective_target, (1 << 20) * 19 / 20 - 100);
    /// # std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub reserved_bytes: u64,
    /// RAM the cold stubs cost (Σ `ENTRY_OVERHEAD + key heap`) — an
    /// estimate for the gauge; the stubs are charged in `used_memory`
    /// through the keyspace table and the key bytes, so this is part of
    /// it, not in addition to it.
    ///
    /// ```
    /// use kevy_store::{ENTRY_OVERHEAD, SetCondition, Store};
    /// # let dir = std::env::temp_dir().join(format!("kevy-doc-stub-{}", std::process::id()));
    /// let mut s = Store::new();
    /// s.enable_tiering(&dir, 1 << 20)?;
    /// s.set(b"k", vec![b'x'; 4096], None, SetCondition::Always);
    /// s.set_tier_budget(1);
    /// s.demote_to_watermark();
    /// // a short key lives inline, so the stub costs the entry overhead alone
    /// assert_eq!(s.tier_stats().stub_bytes, ENTRY_OVERHEAD);
    /// # std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub stub_bytes: u64,
    /// Keys demoted to the cold tier since boot.
    ///
    /// ```
    /// use kevy_store::{SetCondition, Store};
    /// # let dir = std::env::temp_dir().join(format!("kevy-doc-demotions-{}", std::process::id()));
    /// let mut s = Store::new();
    /// s.enable_tiering(&dir, 1 << 20)?;
    /// s.set(b"a", vec![b'x'; 4096], None, SetCondition::Always);
    /// s.set(b"b", vec![b'y'; 4096], None, SetCondition::Always);
    /// s.set_tier_budget(1);
    /// s.demote_to_watermark();
    /// assert_eq!(s.tier_stats().demotions_total, 2);
    /// # std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub demotions_total: u64,
    /// Keys promoted back since boot.
    ///
    /// ```
    /// use kevy_store::{SetCondition, Store};
    /// # let dir = std::env::temp_dir().join(format!("kevy-doc-promotions-{}", std::process::id()));
    /// let mut s = Store::new();
    /// s.enable_tiering(&dir, 1 << 20)?;
    /// s.set(b"k", vec![b'x'; 4096], None, SetCondition::Always);
    /// s.set_tier_budget(1);
    /// s.demote_to_watermark();
    /// s.get(b"k")?; // first read serves from disk without promoting
    /// assert_eq!(s.tier_stats().promotions_total, 0);
    /// s.get(b"k")?; // the second promotes
    /// assert_eq!(s.tier_stats().promotions_total, 1);
    /// # std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub promotions_total: u64,
    /// Vlog record reads (serve + promote + peek).
    ///
    /// ```
    /// use kevy_store::{SetCondition, Store};
    /// # let dir = std::env::temp_dir().join(format!("kevy-doc-preads-{}", std::process::id()));
    /// let mut s = Store::new();
    /// s.enable_tiering(&dir, 1 << 20)?;
    /// s.set(b"k", vec![b'x'; 4096], None, SetCondition::Always);
    /// s.set_tier_budget(1);
    /// s.demote_to_watermark();
    /// assert_eq!(s.get(b"k")?.map(|v| v.len()), Some(4096));
    /// assert_eq!(s.tier_stats().preads_total, 1);
    /// # std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub preads_total: u64,
    /// No-promote peek record reads only — one per cold row.
    ///
    /// ```
    /// use kevy_store::Store;
    /// # let dir = std::env::temp_dir().join(format!("kevy-doc-peek-{}", std::process::id()));
    /// let mut s = Store::new();
    /// s.enable_tiering(&dir, 1 << 20)?;
    /// s.hset(b"row", &[(b"a".as_slice(), [b'x'; 4096].as_slice()), (b"b", b"1")])?;
    /// s.set_tier_budget(1);
    /// s.demote_to_watermark();
    /// // two fields, one record read
    /// let row = s.peek_hash_fields(b"row", &[b"a".as_slice(), b"b"])?.expect("row exists");
    /// assert_eq!(row[1].as_deref(), Some(&b"1"[..]));
    /// assert_eq!(s.tier_stats().peek_preads_total, 1);
    /// # std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub peek_preads_total: u64,
    /// Batched cold-read submissions — one per page batch on
    /// the sync reader; kernel submit count on the uring reader.
    ///
    /// ```
    /// use kevy_store::{Store, SyncColdRead};
    /// # let dir = std::env::temp_dir().join(format!("kevy-doc-batch-{}", std::process::id()));
    /// let mut s = Store::new();
    /// s.enable_tiering(&dir, 1 << 20)?;
    /// s.hset(b"r1", &[(b"f".as_slice(), [b'x'; 4096].as_slice())])?;
    /// s.hset(b"r2", &[(b"f".as_slice(), [b'y'; 4096].as_slice())])?;
    /// s.set_tier_budget(1);
    /// s.demote_to_watermark();
    /// let rows = s.peek_hash_rows(&[b"r1".as_slice(), b"r2"], &[b"f".as_slice()], &mut SyncColdRead);
    /// assert_eq!(rows.len(), 2);
    /// assert_eq!(s.tier_stats().batch_submissions_total, 1, "both rows in one batch");
    /// # std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub batch_submissions_total: u64,
    /// Currently-cold keys.
    ///
    /// ```
    /// use kevy_store::{SetCondition, Store};
    /// # let dir = std::env::temp_dir().join(format!("kevy-doc-coldkeys-{}", std::process::id()));
    /// let mut s = Store::new();
    /// s.enable_tiering(&dir, 1 << 20)?;
    /// s.set(b"k", vec![b'x'; 4096], None, SetCondition::Always);
    /// s.set_tier_budget(1);
    /// s.demote_to_watermark();
    /// assert_eq!(s.tier_stats().cold_keys, 1);
    /// s.del(&[b"k".as_slice()]);
    /// assert_eq!(s.tier_stats().cold_keys, 0);
    /// # std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub cold_keys: u64,
    /// Σ original weights of currently-cold values.
    ///
    /// ```
    /// use kevy_store::{SetCondition, Store};
    /// # let dir = std::env::temp_dir().join(format!("kevy-doc-coldbytes-{}", std::process::id()));
    /// let mut s = Store::new();
    /// s.enable_tiering(&dir, 1 << 20)?;
    /// s.set(b"k", vec![b'x'; 4096], None, SetCondition::Always);
    /// s.set_tier_budget(1);
    /// s.demote_to_watermark();
    /// // the value's in-RAM weight at demotion, not its compressed size on disk
    /// let st = s.tier_stats();
    /// assert!(st.cold_bytes >= 4096 && st.cold_bytes > st.vlog_bytes);
    /// # std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub cold_bytes: u64,
    /// Vlog file count.
    ///
    /// ```
    /// use kevy_store::Store;
    /// # let dir = std::env::temp_dir().join(format!("kevy-doc-vfiles-{}", std::process::id()));
    /// let mut s = Store::new();
    /// s.enable_tiering(&dir, 1 << 20)?;
    /// assert_eq!(s.tier_stats().vlog_files, 1, "the active file opens with the tier");
    /// # std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub vlog_files: u64,
    /// Vlog total bytes on disk.
    ///
    /// ```
    /// use kevy_store::{SetCondition, Store};
    /// # let dir = std::env::temp_dir().join(format!("kevy-doc-vbytes-{}", std::process::id()));
    /// let mut s = Store::new();
    /// s.enable_tiering(&dir, 1 << 20)?;
    /// assert_eq!(s.tier_stats().vlog_bytes, 0);
    /// s.set(b"k", vec![b'x'; 4096], None, SetCondition::Always);
    /// s.set_tier_budget(1);
    /// s.demote_to_watermark();
    /// assert!(s.tier_stats().vlog_bytes > 0);
    /// # std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub vlog_bytes: u64,
    /// Vlog live (non-dead) bytes.
    ///
    /// ```
    /// use kevy_store::{SetCondition, Store};
    /// # let dir = std::env::temp_dir().join(format!("kevy-doc-vlive-{}", std::process::id()));
    /// let mut s = Store::new();
    /// s.enable_tiering(&dir, 1 << 20)?;
    /// s.set(b"k", vec![b'x'; 4096], None, SetCondition::Always);
    /// s.set_tier_budget(1);
    /// s.demote_to_watermark();
    /// assert_eq!(s.tier_stats().vlog_live_bytes, s.tier_stats().vlog_bytes);
    /// // deleting the key kills its record; the bytes stay until compaction
    /// s.del(&[b"k".as_slice()]);
    /// assert_eq!(s.tier_stats().vlog_live_bytes, 0);
    /// assert!(s.tier_stats().vlog_bytes > 0);
    /// # std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub vlog_live_bytes: u64,
    /// Vlog compaction epoch (retired-file counter).
    ///
    /// ```
    /// use kevy_store::{SetCondition, Store};
    /// # let dir = std::env::temp_dir().join(format!("kevy-doc-epoch-{}", std::process::id()));
    /// let mut s = Store::new();
    /// s.enable_tiering(&dir, 1 << 20)?;
    /// s.set(b"k", vec![b'x'; 4096], None, SetCondition::Always);
    /// s.set_tier_budget(1);
    /// s.demote_to_watermark();
    /// s.del(&[b"k".as_slice()]);
    /// s.tier_compact_tick();
    /// // the only file is still the active one, so nothing has been retired
    /// assert_eq!(s.tier_stats().vlog_epoch, 0);
    /// # std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub vlog_epoch: u64,
}
