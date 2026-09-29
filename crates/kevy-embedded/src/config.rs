//! Embedded-store configuration. Builder-style — every knob has a sane
//! default so `Config::default()` works for the simplest use case
//! (in-memory, no persistence, background TTL reaper).

#[cfg(feature = "persist")]
use std::path::PathBuf;
use std::time::Duration;

#[cfg(feature = "persist")]
pub use kevy_persist::Fsync as AppendFsync;
#[cfg(feature = "persist")]
pub use kevy_persist::ReplayMode;
pub use kevy_store::EvictionPolicy;

#[cfg(feature = "tier")]
pub use crate::config_tier::TierBudgetSpec;

pub use crate::modes::TtlReaperMode;

/// Embedded-store config. Build by chaining `with_*` methods on
/// [`Config::default`], or assign its public fields.
///
/// ```
/// use kevy_embedded::{Config, Store};
///
/// let mut cfg = Config::default().with_ttl_reaper_manual();
/// cfg.reaper_samples = 40;
/// let store = Store::open(cfg)?;
/// # Ok::<(), kevy_embedded::KevyError>(())
/// ```
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Config {
    /// Optional READ-ONLY RESP listener address (ops tooling —
    /// redis-cli against a live embedded store). `None` (default) =
    /// no listener thread, no socket, zero tax.
    ///
    /// ```
    /// let addr: std::net::SocketAddr = "127.0.0.1:6009".parse()?;
    /// assert_eq!(kevy_embedded::Config::default().resp_listener, None);
    /// let cfg = kevy_embedded::Config::default().with_resp_listener(addr);
    /// assert_eq!(cfg.resp_listener, Some(addr));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[cfg(feature = "listener")]
    pub resp_listener: Option<std::net::SocketAddr>,
    /// Soft memory ceiling in bytes. `0` (default) = unlimited.
    ///
    /// ```
    /// let cfg = kevy_embedded::Config::default().with_max_memory(64 << 20);
    /// assert_eq!(cfg.maxmemory, 64 << 20);
    /// assert_eq!(kevy_embedded::Config::default().maxmemory, 0); // unlimited
    /// ```
    pub maxmemory: u64,
    /// Eviction policy when over `maxmemory`. Default `NoEviction`.
    ///
    /// ```
    /// use kevy_embedded::{Config, EvictionPolicy, Store};
    ///
    /// let cfg = Config::default()
    ///     .with_max_memory(256 << 10)
    ///     .with_eviction(EvictionPolicy::AllKeysLru);
    /// assert_eq!(cfg.eviction_policy, EvictionPolicy::AllKeysLru);
    /// let s = Store::open(cfg)?;
    /// for i in 0..2000 {
    ///     s.set(format!("k{i}").as_bytes(), &[0u8; 512])?;
    /// }
    /// assert!(s.info().evictions > 0, "old keys made room for new ones");
    /// # Ok::<(), kevy_embedded::KevyError>(())
    /// ```
    pub eviction_policy: EvictionPolicy,
    /// Persistence directory. `None` = pure in-memory (no AOF, no snapshot).
    ///
    /// ```
    /// use kevy_embedded::{Config, Store};
    ///
    /// let dir = kevy_tmpdir::TmpDir::new("config-data-dir");
    /// let cfg = Config::default().with_persist(dir.path());
    /// assert_eq!(cfg.data_dir.as_deref(), Some(dir.path()));
    /// Store::open(cfg.clone())?.set(b"k", b"v")?;
    /// assert_eq!(Store::open(cfg)?.get(b"k")?.as_deref(), Some(&b"v"[..]));
    /// # Ok::<(), kevy_embedded::KevyError>(())
    /// ```
    #[cfg(feature = "persist")]
    pub data_dir: Option<PathBuf>,
    /// AOF on/off when `data_dir` is set. Defaults to `true` (on) when
    /// `with_persist` was called; ignored if `data_dir` is `None`.
    ///
    /// ```
    /// use kevy_embedded::{Config, Store};
    ///
    /// let dir = kevy_tmpdir::TmpDir::new("config-aof");
    /// let cfg = Config::default().with_persist(dir.path()).without_aof();
    /// assert!(!cfg.aof);
    /// let s = Store::open(cfg)?;
    /// s.set(b"k", b"v")?;
    /// assert_eq!(s.info().aof_bytes, 0); // nothing is logged
    /// # Ok::<(), kevy_embedded::KevyError>(())
    /// ```
    #[cfg(feature = "persist")]
    pub aof: bool,
    /// AOF fsync policy. Default `EverySec` (matches Redis: ≤ 1 s loss).
    ///
    /// ```
    /// use kevy_embedded::{AppendFsync, Config};
    ///
    /// assert_eq!(Config::default().appendfsync, AppendFsync::EverySec);
    /// let cfg = Config::default().with_appendfsync(AppendFsync::Always);
    /// assert_eq!(cfg.appendfsync, AppendFsync::Always);
    /// ```
    #[cfg(feature = "persist")]
    pub appendfsync: AppendFsync,
    /// Transparent-tiering RAM budget (capacity arc). `None` (default)
    /// = tiering off — today's paths byte-identical. `Some` requires a
    /// disk `data_dir` (the cold value log lives at `<data_dir>/tier/`);
    /// a mem-only store rejects the combo at open. The budget is the
    /// WHOLE store's (split evenly across shards); auto/percent forms
    /// resolve against the detected memory bound at open and re-resolve
    /// on every reaper tick.
    ///
    /// ```
    /// use kevy_embedded::Config;
    ///
    /// assert!(Config::default().tier_budget.is_none()); // tiering off
    /// let cfg = Config::default().with_tier_budget(64 << 20);
    /// assert!(cfg.tier_budget.is_some());
    /// ```
    #[cfg(feature = "tier")]
    pub tier_budget: Option<TierBudgetSpec>,
    /// Largest value the tier may spill (bytes; 0 = unlimited). Default
    /// 256 KiB (RFC §7): an embedded cold read holds the shard lock for
    /// the pread, so the cap bounds that hold time. Over-cap values
    /// simply stay hot.
    ///
    /// ```
    /// let mut cfg = kevy_embedded::Config::default();
    /// assert_eq!(cfg.max_spill_value, 256 << 10);
    /// cfg.max_spill_value = 0; // any value may spill
    /// assert_eq!(cfg.max_spill_value, 0);
    /// ```
    pub max_spill_value: u64,
    /// TTL reaper mode. Default `Background`.
    ///
    /// ```
    /// use kevy_embedded::{Config, Store, TtlReaperMode};
    ///
    /// let cfg = Config::default().with_ttl_reaper_manual();
    /// assert_eq!(cfg.ttl_reaper, TtlReaperMode::Manual);
    /// let s = Store::open(cfg)?;
    /// s.tick(); // the caller drives the reaper
    /// # Ok::<(), kevy_embedded::KevyError>(())
    /// ```
    pub ttl_reaper: TtlReaperMode,
    /// Reaper tick interval. Default 100 ms (10 Hz).
    ///
    /// ```
    /// use std::time::Duration;
    ///
    /// let cfg = kevy_embedded::Config::default().with_reaper_interval(Duration::from_millis(10));
    /// assert_eq!(cfg.reaper_interval, Duration::from_millis(10));
    /// ```
    pub reaper_interval: Duration,
    /// `tick_expire` samples per round. Default 20 (matches Redis).
    ///
    /// ```
    /// let mut cfg = kevy_embedded::Config::default();
    /// assert_eq!(cfg.reaper_samples, 20);
    /// cfg.reaper_samples = 100; // reap harder per round
    /// assert_eq!(cfg.reaper_samples, 100);
    /// ```
    pub reaper_samples: usize,
    /// Max sample rounds per tick. Default 16.
    ///
    /// ```
    /// let mut cfg = kevy_embedded::Config::default();
    /// assert_eq!(cfg.reaper_max_rounds, 16);
    /// cfg.reaper_max_rounds = 4; // bound each tick's work
    /// assert_eq!(cfg.reaper_max_rounds, 4);
    /// ```
    pub reaper_max_rounds: u32,
    /// Auto-`BGREWRITEAOF` trigger: rewrite when the live AOF has grown by at
    /// least this percent over its size at the previous rewrite. `0` disables
    /// (call [`crate::Store::rewrite_aof`] manually). Default `100` (Redis).
    ///
    /// ```
    /// let cfg = kevy_embedded::Config::default().with_auto_aof_rewrite(50, 1 << 20);
    /// assert_eq!((cfg.auto_aof_rewrite_pct, cfg.auto_aof_rewrite_min_size), (50, 1 << 20));
    /// assert_eq!(kevy_embedded::Config::default().auto_aof_rewrite_pct, 100);
    /// ```
    #[cfg(feature = "persist")]
    pub auto_aof_rewrite_pct: u32,
    /// Floor below which auto-rewrite is skipped. Default `64 MiB` (Redis).
    ///
    /// ```
    /// let cfg = kevy_embedded::Config::default();
    /// assert_eq!(cfg.auto_aof_rewrite_min_size, 64 << 20);
    /// let cfg = cfg.with_auto_aof_rewrite(100, 8 << 20);
    /// assert_eq!(cfg.auto_aof_rewrite_min_size, 8 << 20);
    /// ```
    #[cfg(feature = "persist")]
    pub auto_aof_rewrite_min_size: u64,
    /// Absolute-size auto-rewrite trigger in bytes (0 = off). The growth
    /// rule alone lets a large log double before compacting — a 2.2 GB AOF
    /// waits for 4.4 GB; this caps it outright.
    ///
    /// ```
    /// let mut cfg = kevy_embedded::Config::default();
    /// assert_eq!(cfg.auto_aof_rewrite_bytes, 0); // off
    /// cfg.auto_aof_rewrite_bytes = 1 << 30; // compact once the log reaches 1 GiB
    /// assert_eq!(cfg.auto_aof_rewrite_bytes, 1 << 30);
    /// ```
    pub auto_aof_rewrite_bytes: u64,
    /// Time-based auto-rewrite trigger in seconds (0 = off): compact at
    /// least this often while the log grows.
    ///
    /// ```
    /// let mut cfg = kevy_embedded::Config::default();
    /// assert_eq!(cfg.auto_aof_rewrite_interval_secs, 0); // off
    /// cfg.auto_aof_rewrite_interval_secs = 3600; // compact at least hourly
    /// assert_eq!(cfg.auto_aof_rewrite_interval_secs, 3600);
    /// ```
    pub auto_aof_rewrite_interval_secs: u64,
    /// What replay does at a corrupt v2 record: stop there
    /// ([`ReplayMode::Strict`], the default), or hop to the next valid
    /// record (length + CRC + parse all agree) instead of dropping the
    /// good tail behind it ([`ReplayMode::Resync`]).
    ///
    /// ```
    /// use kevy_embedded::{Config, ReplayMode};
    ///
    /// assert_eq!(Config::default().replay_mode, ReplayMode::Strict);
    /// let cfg = Config::default().with_replay_mode(ReplayMode::Resync);
    /// assert_eq!(cfg.replay_mode, ReplayMode::Resync);
    /// ```
    #[cfg(feature = "persist")]
    pub replay_mode: ReplayMode,
    /// Size in bytes of each shard's staging ring (0 = off): appends land
    /// in a shared file mapping, so a process that is killed keeps every
    /// write that returned. Only `EverySec` and `No` stage. Default 4 MiB.
    ///
    /// ```
    /// let cfg = kevy_embedded::Config::default();
    /// assert_eq!(cfg.stage_bytes, 4 << 20);
    /// assert_eq!(cfg.with_stage_ring(0).stage_bytes, 0); // staging off
    /// ```
    pub stage_bytes: u64,
    /// Append by copying into a mapping of the AOF's preallocated tail
    /// instead of `write()`, under `EverySec` and `No`. It keeps a killed
    /// process's writes as the staging ring does, and replaces it. On by
    /// default on Apple platforms only, where a fresh mapped page is cheap;
    /// on Linux the page fault makes it slower than `write()`.
    ///
    /// ```
    /// let cfg = kevy_embedded::Config::default();
    /// assert_eq!(cfg.mapped_aof, cfg!(target_vendor = "apple"));
    /// assert!(!cfg.with_mapped_aof(false).mapped_aof);
    /// ```
    pub mapped_aof: bool,
    /// Optional push-style metric callback (replay / rewrite events). Default
    /// `None`. Set via [`Self::with_metric_sink`]; not part of `Debug` output.
    #[cfg(feature = "persist")]
    pub(crate) metric_sink: Option<crate::metric::MetricSink>,
    /// Keyspace shard count (`hash(key) % shards`), each a fully independent
    /// lock + keyspace + AOF (shared-nothing) — concurrent access scales across
    /// cores. **Default `1`** (single shard = the original single-lock /
    /// single-`aof-0.aof` layout, zero migration). Set `> 1` via
    /// [`Self::with_shards`]; the first open with `> 1` re-shards an existing
    /// single AOF into per-shard files.
    ///
    /// ```
    /// use kevy_embedded::{Config, Store};
    ///
    /// assert_eq!(Config::default().with_shards(0).shards, 1); // clamped
    /// let s = Store::open(Config::default().with_shards(4))?;
    /// s.set(b"a", b"1")?;
    /// s.set(b"b", b"2")?;
    /// assert_eq!(s.info().keys, 2); // counted across all four shards
    /// # Ok::<(), kevy_embedded::KevyError>(())
    /// ```
    pub shards: usize,
    /// Replication upstream. `Some("host:port")` makes this store a
    /// read-replica that streams writes from the named primary; `None`
    /// (default) is a normal primary store. Configured via
    /// [`Self::with_replica_upstream`] or the convenience constructor
    /// [`crate::Store::open_replica`].
    ///
    /// ```
    /// let cfg = kevy_embedded::Config::default().with_replica_upstream("10.0.0.5:6380");
    /// assert_eq!(cfg.replica_upstream.as_deref(), Some("10.0.0.5:6380"));
    /// assert!(kevy_embedded::Config::default().replica_upstream.is_none());
    /// ```
    #[cfg(feature = "replicate")]
    pub replica_upstream: Option<String>,
    /// Replica identity string sent to the primary at handshake
    /// (`REPLICATE FROM <offset> ID <replica_id>`). Default
    /// `"kevy-embedded-replica"`. Override per-process when multiple
    /// embed replicas connect to the same primary (they'd otherwise
    /// share the slot and clobber each other's session state on the
    /// primary side).
    ///
    /// ```
    /// let cfg = kevy_embedded::Config::default();
    /// assert_eq!(cfg.replica_id, "kevy-embedded-replica");
    /// assert_eq!(cfg.with_replica_id("edge-7").replica_id, "edge-7");
    /// ```
    #[cfg(feature = "replicate")]
    pub replica_id: String,
    /// Replica reconnect backoff: lower bound. Default 100 ms. The
    /// runner sleeps this long after the first connection failure;
    /// each subsequent failure doubles the wait up to
    /// [`Self::replica_reconnect_max`].
    ///
    /// ```
    /// use std::time::Duration;
    ///
    /// let cfg = kevy_embedded::Config::default()
    ///     .with_replica_reconnect(Duration::from_millis(50), Duration::from_secs(2));
    /// assert_eq!(cfg.replica_reconnect_min, Duration::from_millis(50));
    /// ```
    #[cfg(feature = "replicate")]
    pub replica_reconnect_min: Duration,
    /// Replica reconnect backoff: upper bound. Default 5 s — matches
    /// the server-side replica reconnect default so embed replicas and
    /// server replicas behave identically when the same primary
    /// disappears.
    ///
    /// ```
    /// use std::time::Duration;
    ///
    /// let min = Duration::from_secs(1);
    /// let cfg = kevy_embedded::Config::default().with_replica_reconnect(min, Duration::from_millis(10));
    /// assert_eq!(cfg.replica_reconnect_max, min); // never below the lower bound
    /// ```
    #[cfg(feature = "replicate")]
    pub replica_reconnect_max: Duration,
    /// Embed-as-writer bind address (`"host:port"` or
    /// `"0.0.0.0:port"`) for the replication source listener. When
    /// `Some`, every commit on this store pushes its argv into a
    /// process-local `ReplicationSource` backlog, and replicas (other
    /// embeds, server-as-replicas) connect to this port to stream the
    /// writes. `None` (default) keeps the embed in pure-local mode.
    /// Mutually exclusive in spirit with `replica_upstream` (a single
    /// store should be either a writer source or a reader sink, not
    /// both); the builder does not reject the combo so tests can
    /// exercise the guard rails.
    ///
    /// ```
    /// let cfg = kevy_embedded::Config::default().with_embed_writer("127.0.0.1:6390");
    /// assert_eq!(cfg.embed_writer_listen_addr.as_deref(), Some("127.0.0.1:6390"));
    /// assert!(kevy_embedded::Config::default().embed_writer_listen_addr.is_none());
    /// ```
    #[cfg(feature = "replicate")]
    pub embed_writer_listen_addr: Option<String>,
    /// Noise keys for either replication direction; both plaintext by
    /// default. Set via [`Self::with_replica_security`] and
    /// [`Self::with_writer_security`].
    #[cfg(feature = "replicate")]
    pub(crate) link_security: crate::config_secure::LinkSecurity,
    /// CDC feed (changes_since / changes_tail). Default off.
    ///
    /// ```
    /// use kevy_embedded::{Config, Store};
    ///
    /// let cfg = Config::default().with_feed(0);
    /// assert!(cfg.feed_enabled);
    /// let s = Store::open(cfg)?;
    /// assert!(s.changes_tail().is_ok()); // a disabled feed answers Disabled
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[cfg(feature = "replicate")]
    pub feed_enabled: bool,
    /// Feed backlog byte budget. Default 64 MB, capped at 1 GB.
    ///
    /// ```
    /// let cfg = kevy_embedded::Config::default().with_feed(0);
    /// assert_eq!(cfg.feed_buffer_size, 64 << 20);
    /// let cfg = kevy_embedded::Config::default().with_feed(4 << 30);
    /// assert_eq!(cfg.feed_buffer_size, 1 << 30); // capped at 1 GB
    /// ```
    #[cfg(feature = "replicate")]
    pub feed_buffer_size: u64,
    /// Backlog byte budget for the embed-as-writer source. Default
    /// `1 MiB` (matches the server replication default).
    /// Set higher when consumers may disconnect for longer than
    /// the backlog can buffer (otherwise a reconnect falls past the
    /// backlog and is re-seeded with a full snapshot ship instead of
    /// an incremental stream).
    ///
    /// ```
    /// let cfg = kevy_embedded::Config::default();
    /// assert_eq!(cfg.embed_writer_backlog_bytes, 1 << 20);
    /// assert_eq!(cfg.with_embed_writer_backlog(1).embed_writer_backlog_bytes, 64 << 10); // floor
    /// ```
    #[cfg(feature = "replicate")]
    pub embed_writer_backlog_bytes: usize,
}

impl Default for Config {
    // LOC-WAIVER: pure field-default table, one line (and its cfg) per field
    fn default() -> Self {
        Self {
            #[cfg(feature = "listener")]
            resp_listener: None,
            maxmemory: 0,
            eviction_policy: EvictionPolicy::NoEviction,
            #[cfg(feature = "persist")]
            data_dir: None,
            #[cfg(feature = "persist")]
            aof: true,
            #[cfg(feature = "persist")]
            appendfsync: AppendFsync::EverySec,
            #[cfg(feature = "tier")]
            tier_budget: None,
            max_spill_value: 256 << 10,
            ttl_reaper: TtlReaperMode::Background,
            reaper_interval: Duration::from_millis(100),
            reaper_samples: 20,
            reaper_max_rounds: 16,
            #[cfg(feature = "persist")]
            auto_aof_rewrite_pct: 100,
            #[cfg(feature = "persist")]
            auto_aof_rewrite_min_size: 64 * 1024 * 1024,
            auto_aof_rewrite_bytes: 0,
            auto_aof_rewrite_interval_secs: 0,
            #[cfg(feature = "persist")]
            replay_mode: ReplayMode::Strict,
            stage_bytes: 4 * 1024 * 1024,
            mapped_aof: cfg!(target_vendor = "apple"),
            #[cfg(feature = "persist")]
            metric_sink: None,
            shards: 1,
            #[cfg(feature = "replicate")]
            replica_upstream: None,
            #[cfg(feature = "replicate")]
            replica_id: String::from("kevy-embedded-replica"),
            #[cfg(feature = "replicate")]
            replica_reconnect_min: Duration::from_millis(100),
            #[cfg(feature = "replicate")]
            replica_reconnect_max: Duration::from_secs(5),
            #[cfg(feature = "replicate")]
            embed_writer_listen_addr: None,
            #[cfg(feature = "replicate")]
            link_security: Default::default(),
            #[cfg(feature = "replicate")]
            feed_enabled: false,
            #[cfg(feature = "replicate")]
            feed_buffer_size: 64 * 1024 * 1024,
            #[cfg(feature = "replicate")]
            embed_writer_backlog_bytes: 1024 * 1024,
        }
    }
}

#[path = "config_builders.rs"]
mod config_builders;

#[path = "config_tier_builders.rs"]
mod config_tier_builders;
