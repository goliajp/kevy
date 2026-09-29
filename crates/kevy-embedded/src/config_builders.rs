//! The builder methods of [`Config`] (child module via `#[path]`,
//! split from `config.rs` for the 500-LOC house rule; behaviour
//! unchanged).

use super::*;

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
    /// and interval — in one named call This is
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
