//! Embedded-store configuration. Builder-style — every knob has a sane
//! default so `Config::default()` works for the simplest use case
//! (in-memory, no persistence, background TTL reaper).

#[cfg(feature = "persist")]
use std::path::PathBuf;
use std::time::Duration;

#[cfg(feature = "persist")]
pub use kevy_persist::Fsync as AppendFsync;
pub use kevy_store::EvictionPolicy;

#[cfg(feature = "tier")]
pub use crate::config_tier::TierBudgetSpec;

/// How the active TTL reaper runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtlReaperMode {
    /// Spawn a background thread that ticks at the configured interval
    /// (default 100 ms / 10 Hz, matching Redis's `hz=10`). Default.
    Background,
    /// Caller-driven via [`crate::Store::tick`]. Required for WASM
    /// targets (no threads) and single-threaded apps that don't want a
    /// background worker.
    Manual,
}

/// Embedded-store config. Build by chaining `with_*` methods on
/// [`Config::default`].
#[derive(Debug, Clone)]
pub struct Config {
    /// Optional READ-ONLY RESP listener address (ops tooling —
    /// redis-cli against a live embedded store). `None` (default) =
    /// no listener thread, no socket, zero tax.
    #[cfg(feature = "listener")]
    pub resp_listener: Option<std::net::SocketAddr>,
    /// Soft memory ceiling in bytes. `0` (default) = unlimited.
    pub maxmemory: u64,
    /// Eviction policy when over `maxmemory`. Default `NoEviction`.
    pub eviction_policy: EvictionPolicy,
    /// Persistence directory. `None` = pure in-memory (no AOF, no snapshot).
    #[cfg(feature = "persist")]
    pub data_dir: Option<PathBuf>,
    /// AOF on/off when `data_dir` is set. Defaults to `true` (on) when
    /// `with_persist` was called; ignored if `data_dir` is `None`.
    #[cfg(feature = "persist")]
    pub aof: bool,
    /// AOF fsync policy. Default `EverySec` (matches Redis: ≤ 1 s loss).
    #[cfg(feature = "persist")]
    pub appendfsync: AppendFsync,
    /// Transparent-tiering RAM budget (capacity arc). `None` (default)
    /// = tiering off — today's paths byte-identical. `Some` requires a
    /// disk `data_dir` (the cold value log lives at `<data_dir>/tier/`);
    /// a mem-only store rejects the combo at open. The budget is the
    /// WHOLE store's (split evenly across shards); auto/percent forms
    /// resolve against the detected memory bound at open and re-resolve
    /// on every reaper tick.
    #[cfg(feature = "tier")]
    pub tier_budget: Option<TierBudgetSpec>,
    /// Largest value the tier may spill (bytes; 0 = unlimited). Default
    /// 256 KiB (RFC §7): an embedded cold read holds the shard lock for
    /// the pread, so the cap bounds that hold time. Over-cap values
    /// simply stay hot.
    pub max_spill_value: u64,
    /// TTL reaper mode. Default `Background`.
    pub ttl_reaper: TtlReaperMode,
    /// Reaper tick interval. Default 100 ms (10 Hz).
    pub reaper_interval: Duration,
    /// `tick_expire` samples per round. Default 20 (matches Redis).
    pub reaper_samples: usize,
    /// Max sample rounds per tick. Default 16.
    pub reaper_max_rounds: u32,
    /// Auto-`BGREWRITEAOF` trigger: rewrite when the live AOF has grown by at
    /// least this percent over its size at the previous rewrite. `0` disables
    /// (call [`crate::Store::rewrite_aof`] manually). Default `100` (Redis).
    #[cfg(feature = "persist")]
    pub auto_aof_rewrite_pct: u32,
    /// Floor below which auto-rewrite is skipped. Default `64 MiB` (Redis).
    #[cfg(feature = "persist")]
    pub auto_aof_rewrite_min_size: u64,
    /// Absolute-size auto-rewrite trigger in bytes (0 = off). The growth
    /// rule alone lets a large log double before compacting — a 2.2 GB AOF
    /// waits for 4.4 GB; this caps it outright.
    pub auto_aof_rewrite_bytes: u64,
    /// Time-based auto-rewrite trigger in seconds (0 = off): compact at
    /// least this often while the log grows.
    pub auto_aof_rewrite_interval_secs: u64,
    /// Best-effort replay: on a corrupt v2 record, hop to the next valid
    /// record (length + CRC + parse all agree) instead of dropping the
    /// good tail behind it. Default false (strict).
    pub replay_resync: bool,
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
    pub shards: usize,
    /// Replication upstream. `Some("host:port")` makes this store a
    /// read-replica that streams writes from the named primary; `None`
    /// (default) is a normal primary store. Configured via
    /// [`Self::with_replica_upstream`] or the convenience constructor
    /// [`crate::Store::open_replica`].
    #[cfg(feature = "replicate")]
    pub replica_upstream: Option<String>,
    /// Replica identity string sent to the primary at handshake
    /// (`REPLICATE FROM <offset> ID <replica_id>`). Default
    /// `"kevy-embedded-replica"`. Override per-process when multiple
    /// embed replicas connect to the same primary (they'd otherwise
    /// share the slot and clobber each other's session state on the
    /// primary side).
    #[cfg(feature = "replicate")]
    pub replica_id: String,
    /// Replica reconnect backoff: lower bound. Default 100 ms. The
    /// runner sleeps this long after the first connection failure;
    /// each subsequent failure doubles the wait up to
    /// [`Self::replica_reconnect_max`].
    #[cfg(feature = "replicate")]
    pub replica_reconnect_min: Duration,
    /// Replica reconnect backoff: upper bound. Default 5 s — matches
    /// the server-side replica reconnect default so embed replicas and
    /// server replicas behave identically when the same primary
    /// disappears.
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
    #[cfg(feature = "replicate")]
    pub embed_writer_listen_addr: Option<String>,
    /// CDC feed (changes_since / changes_tail). Default off.
    #[cfg(feature = "replicate")]
    pub feed_enabled: bool,
    /// Feed backlog byte budget. Default 64 MB, capped at 1 GB.
    #[cfg(feature = "replicate")]
    pub feed_buffer_size: u64,
    /// Backlog byte budget for the embed-as-writer source. Default
    /// `1 MiB` (matches the server replication default).
    /// Set higher when consumers may disconnect for longer than
    /// the backlog can buffer (otherwise a reconnect falls past the
    /// backlog and is re-seeded with a full snapshot ship instead of
    /// an incremental stream).
    #[cfg(feature = "replicate")]
    pub embed_writer_backlog_bytes: usize,
}

impl Default for Config {
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
            replay_resync: false,
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
            feed_enabled: false,
            #[cfg(feature = "replicate")]
            feed_buffer_size: 64 * 1024 * 1024,
            #[cfg(feature = "replicate")]
            embed_writer_backlog_bytes: 1024 * 1024,
        }
    }
}

impl Config {
    /// Enable the read-only RESP listener on `addr`
    /// (e.g. `"127.0.0.1:6009".parse().unwrap()`).
    #[cfg(feature = "listener")]
    #[must_use]
    pub fn with_resp_listener(mut self, addr: std::net::SocketAddr) -> Self {
        self.resp_listener = Some(addr);
        self
    }

    /// Enable persistence under `dir` — snapshot file + AOF land inside.
    /// AOF defaults on; turn it off with [`Self::without_aof`] for pure
    /// snapshot-only durability.
    #[cfg(feature = "persist")]
    #[must_use]
    pub fn with_persist(mut self, dir: impl Into<PathBuf>) -> Self {
        self.data_dir = Some(dir.into());
        self
    }

    /// Disable the AOF (snapshot-only persistence — explicit `save_snapshot`
    /// calls are the only way data survives restart).
    #[cfg(feature = "persist")]
    #[must_use]
    pub fn without_aof(mut self) -> Self {
        self.aof = false;
        self
    }

    /// Soft memory ceiling in bytes. `0` keeps the default (unlimited).
    #[must_use]
    pub fn with_max_memory(mut self, bytes: u64) -> Self {
        self.maxmemory = bytes;
        self
    }

    /// Eviction policy when over [`Self::with_max_memory`].
    #[must_use]
    pub fn with_eviction(mut self, policy: EvictionPolicy) -> Self {
        self.eviction_policy = policy;
        self
    }

    /// AOF fsync policy. Default [`AppendFsync::EverySec`].
    #[cfg(feature = "persist")]
    #[must_use]
    pub fn with_appendfsync(mut self, fsync: AppendFsync) -> Self {
        self.appendfsync = fsync;
        self
    }

    /// Absolute-size auto-rewrite trigger: compact whenever the AOF reaches
    /// `bytes`, regardless of growth ratio (0 = off). Complements
    /// [`Self::with_auto_aof_rewrite`], whose growth rule is too sluggish
    /// for long-lived instances with a large baseline.
    #[cfg(feature = "persist")]
    #[must_use]
    pub fn with_auto_rewrite_bytes(mut self, bytes: u64) -> Self {
        self.auto_aof_rewrite_bytes = bytes;
        self
    }

    /// Time-based auto-rewrite trigger: compact at least every `interval`
    /// while the log grows (zero duration = off).
    #[cfg(feature = "persist")]
    #[must_use]
    pub fn with_auto_rewrite_interval(mut self, interval: std::time::Duration) -> Self {
        self.auto_aof_rewrite_interval_secs = interval.as_secs();
        self
    }

    /// Best-effort replay: recover the good records BEHIND a corrupt v2
    /// record instead of dropping them (a production incident lost a
    /// 231 MB well-formed tail over one bad frame). The skip is
    /// deterministic — length + CRC32C + an exactly-one-command parse must
    /// all agree before a record is trusted — and the open still reports
    /// `corrupt` so hosts still alert. Strict (default false) remains the
    /// conservative choice: nothing after the first bad byte is trusted.
    #[cfg(feature = "persist")]
    #[must_use]
    pub fn with_replay_resync(mut self, resync: bool) -> Self {
        self.replay_resync = resync;
        self
    }

    /// Auto-`BGREWRITEAOF` thresholds: rewrite once the AOF has grown `pct`
    /// percent past its size at the last rewrite AND is at least `min_size`
    /// bytes. In `Background` reaper mode the check runs on the reaper tick;
    /// in `Manual` mode it runs when you call [`crate::Store::tick`]. Pass
    /// `pct = 0` to disable auto-rewrite (you can still call
    /// [`crate::Store::rewrite_aof`] yourself). Defaults: 100 % / 64 MiB.
    #[cfg(feature = "persist")]
    #[must_use]
    pub fn with_auto_aof_rewrite(mut self, pct: u32, min_size: u64) -> Self {
        self.auto_aof_rewrite_pct = pct;
        self.auto_aof_rewrite_min_size = min_size;
        self
    }

    /// Disable every automatic rewrite trigger — growth, absolute size
    /// and interval — in one named call (v4.1-V7, dogfood F4). This is
    /// the canary-window switch: the first rewrite is the documented
    /// one-way step that upgrades a 3.x-era AOF to v2
    /// ([`crate::Store::downgradeable_to_v3`] reads the window), so an
    /// embedder keeping a binary-swap escape hatch open turns the
    /// automatics off rather than remembering which of three knobs
    /// zeroes which rule. Explicit [`crate::Store::rewrite_aof`] calls
    /// still work — and still close the window.
    #[cfg(feature = "persist")]
    #[must_use]
    pub fn with_auto_aof_rewrite_disabled(mut self) -> Self {
        self.auto_aof_rewrite_pct = 0;
        self.auto_aof_rewrite_bytes = 0;
        self.auto_aof_rewrite_interval_secs = 0;
        self
    }

    /// Shard the keyspace into `n` shared-nothing partitions (`hash(key) % n`),
    /// each with its own lock + keyspace + AOF, so concurrent access scales
    /// across cores. `n` clamps to ≥ 1; `1` (default) is the original
    /// single-shard layout. Going from a single-AOF store to `n > 1`
    /// re-shards the existing `aof-0.aof` into `aof-0..aof-{n-1}` on the next
    /// open (the old file is backed up to `aof-0.aof.premigration.<ts>` first).
    /// Pub/sub is process-wide (handled on shard 0), not sharded.
    #[must_use]
    pub fn with_shards(mut self, n: usize) -> Self {
        self.shards = n.max(1);
        self
    }

    /// Register a push-style metric callback. It receives a [`crate::KevyMetric`] for
    /// each AOF replay (startup) and AOF rewrite (compaction) — wire it to
    /// Prometheus / a log line / a counter. The callback runs synchronously on
    /// the emitting thread (reaper thread for background rewrites), so keep it
    /// fast and non-blocking. Replaces any previously-set sink.
    #[cfg(feature = "persist")]
    #[must_use]
    pub fn with_metric_sink(
        mut self,
        sink: impl Fn(crate::KevyMetric) + Send + Sync + 'static,
    ) -> Self {
        self.metric_sink = Some(crate::metric::MetricSink::new(sink));
        self
    }

    /// Caller-driven TTL reaping — disables the background thread.
    /// Required for WASM (no threads available). Call
    /// [`crate::Store::tick`] yourself from your event loop.
    #[must_use]
    pub fn with_ttl_reaper_manual(mut self) -> Self {
        self.ttl_reaper = TtlReaperMode::Manual;
        self
    }

    /// Configure this store as a replication replica of `upstream`
    /// (`"host:port"` of a kevy server's replication listener). A
    /// background thread streams writes from the primary and applies
    /// them locally; this store rejects local writes with a
    /// `READONLY` error. See [`crate::Store::open_replica`] for the
    /// convenience constructor.
    #[cfg(feature = "replicate")]
    #[must_use]
    pub fn with_replica_upstream(mut self, upstream: impl Into<String>) -> Self {
        self.replica_upstream = Some(upstream.into());
        self
    }

    /// Override the replica identity sent to the primary at handshake.
    /// Useful when multiple embed replicas share one primary —
    /// otherwise they'd share the slot and stomp each other's session
    /// state.
    #[cfg(feature = "replicate")]
    #[must_use]
    pub fn with_replica_id(mut self, id: impl Into<String>) -> Self {
        self.replica_id = id.into();
        self
    }

    /// Override the replica reconnect backoff bounds.
    #[cfg(feature = "replicate")]
    #[must_use]
    pub fn with_replica_reconnect(mut self, min: Duration, max: Duration) -> Self {
        self.replica_reconnect_min = min;
        self.replica_reconnect_max = max.max(min);
        self
    }

    /// Run this store as an embed-as-writer: bind a replication
    /// source listener on `bind_addr` so replicas can subscribe to
    /// the writes applied here.
    #[cfg(feature = "replicate")]
    #[must_use]
    pub fn with_embed_writer(mut self, bind_addr: impl Into<String>) -> Self {
        self.embed_writer_listen_addr = Some(bind_addr.into());
        self
    }

    /// Enable the CDC feed (`changes_since` / `changes_tail`).
    /// `buffer_size` = 0 keeps the 64 MB default; values cap at 1 GB.
    #[cfg(feature = "replicate")]
    #[must_use]
    pub fn with_feed(mut self, buffer_size: u64) -> Self {
        self.feed_enabled = true;
        if buffer_size > 0 {
            self.feed_buffer_size = buffer_size.min(1024 * 1024 * 1024);
        }
        self
    }

    /// Override the embed-as-writer backlog byte budget.
    #[cfg(feature = "replicate")]
    #[must_use]
    pub fn with_embed_writer_backlog(mut self, bytes: usize) -> Self {
        self.embed_writer_backlog_bytes = bytes.max(64 * 1024);
        self
    }

    /// Override the background reaper interval. Default 100 ms.
    #[must_use]
    pub fn with_reaper_interval(mut self, iv: Duration) -> Self {
        self.reaper_interval = iv;
        self
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_pure_in_memory() {
        let c = Config::default();
        assert_eq!(c.maxmemory, 0);
        assert!(c.data_dir.is_none());
        assert_eq!(c.ttl_reaper, TtlReaperMode::Background);
        assert!(c.aof);
    }

    #[test]
    fn builder_chains() {
        let c = Config::default()
            .with_persist("/tmp/foo")
            .with_max_memory(1024)
            .with_eviction(EvictionPolicy::AllKeysLru)
            .with_ttl_reaper_manual()
            .with_appendfsync(AppendFsync::Always);
        assert_eq!(c.data_dir.as_deref(), Some(std::path::Path::new("/tmp/foo")));
        assert_eq!(c.maxmemory, 1024);
        assert_eq!(c.eviction_policy, EvictionPolicy::AllKeysLru);
        assert_eq!(c.ttl_reaper, TtlReaperMode::Manual);
    }

    #[test]
    fn without_aof_disables_logging_path() {
        let c = Config::default().with_persist("/tmp/foo").without_aof();
        assert!(c.data_dir.is_some());
        assert!(!c.aof);
    }
}

#[path = "config_tier_builders.rs"]
mod config_tier_builders;
