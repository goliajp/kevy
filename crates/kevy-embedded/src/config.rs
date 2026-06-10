//! Embedded-store configuration. Builder-style — every knob has a sane
//! default so `Config::default()` works for the simplest use case
//! (in-memory, no persistence, background TTL reaper).

use std::path::PathBuf;
use std::time::Duration;

pub use kevy_persist::Fsync as AppendFsync;
pub use kevy_store::EvictionPolicy;

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
    /// Soft memory ceiling in bytes. `0` (default) = unlimited.
    pub maxmemory: u64,
    /// Eviction policy when over `maxmemory`. Default `NoEviction`.
    pub eviction_policy: EvictionPolicy,
    /// Persistence directory. `None` = pure in-memory (no AOF, no snapshot).
    pub data_dir: Option<PathBuf>,
    /// AOF on/off when `data_dir` is set. Defaults to `true` (on) when
    /// `with_persist` was called; ignored if `data_dir` is `None`.
    pub aof: bool,
    /// AOF fsync policy. Default `EverySec` (matches Redis: ≤ 1 s loss).
    pub appendfsync: AppendFsync,
    /// Snapshot file name inside `data_dir` (single-shard only; `n > 1`
    /// always uses `dump-{i}.rdb`). Default `"dump-0.rdb"`. A custom name
    /// opts the dir out of server interop: no `shards.meta` is recorded,
    /// and a `kevy` server opening the same dir won't find the files.
    pub snapshot_filename: String,
    /// AOF file name inside `data_dir` (single-shard only; `n > 1` always
    /// uses `aof-{i}.aof`). Default `"aof-0.aof"`. Same interop opt-out as
    /// [`Self::snapshot_filename`].
    pub aof_filename: String,
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
    pub auto_aof_rewrite_pct: u32,
    /// Floor below which auto-rewrite is skipped. Default `64 MiB` (Redis).
    pub auto_aof_rewrite_min_size: u64,
    /// Optional push-style metric callback (replay / rewrite events). Default
    /// `None`. Set via [`Self::with_metric_sink`]; not part of `Debug` output.
    pub(crate) metric_sink: Option<crate::metric::MetricSink>,
    /// Keyspace shard count (`hash(key) % shards`), each a fully independent
    /// lock + keyspace + AOF (shared-nothing) — concurrent access scales across
    /// cores. **Default `1`** (single shard = the original single-lock /
    /// single-`aof-0.aof` layout, zero migration). Set `> 1` via
    /// [`Self::with_shards`]; the first open with `> 1` re-shards an existing
    /// single AOF into per-shard files.
    pub shards: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            maxmemory: 0,
            eviction_policy: EvictionPolicy::NoEviction,
            data_dir: None,
            aof: true,
            appendfsync: AppendFsync::EverySec,
            snapshot_filename: String::from("dump-0.rdb"),
            aof_filename: String::from("aof-0.aof"),
            ttl_reaper: TtlReaperMode::Background,
            reaper_interval: Duration::from_millis(100),
            reaper_samples: 20,
            reaper_max_rounds: 16,
            auto_aof_rewrite_pct: 100,
            auto_aof_rewrite_min_size: 64 * 1024 * 1024,
            metric_sink: None,
            shards: 1,
        }
    }
}

impl Config {
    /// Enable persistence under `dir` — snapshot file + AOF land inside.
    /// AOF defaults on; turn it off with [`Self::without_aof`] for pure
    /// snapshot-only durability.
    pub fn with_persist(mut self, dir: impl Into<PathBuf>) -> Self {
        self.data_dir = Some(dir.into());
        self
    }

    /// Disable the AOF (snapshot-only persistence — explicit `save_snapshot`
    /// calls are the only way data survives restart).
    pub fn without_aof(mut self) -> Self {
        self.aof = false;
        self
    }

    /// Soft memory ceiling in bytes. `0` keeps the default (unlimited).
    pub fn with_max_memory(mut self, bytes: u64) -> Self {
        self.maxmemory = bytes;
        self
    }

    /// Eviction policy when over [`Self::with_max_memory`].
    pub fn with_eviction(mut self, policy: EvictionPolicy) -> Self {
        self.eviction_policy = policy;
        self
    }

    /// AOF fsync policy. Default [`AppendFsync::EverySec`].
    pub fn with_appendfsync(mut self, fsync: AppendFsync) -> Self {
        self.appendfsync = fsync;
        self
    }

    /// Auto-`BGREWRITEAOF` thresholds: rewrite once the AOF has grown `pct`
    /// percent past its size at the last rewrite AND is at least `min_size`
    /// bytes. In `Background` reaper mode the check runs on the reaper tick;
    /// in `Manual` mode it runs when you call [`crate::Store::tick`]. Pass
    /// `pct = 0` to disable auto-rewrite (you can still call
    /// [`crate::Store::rewrite_aof`] yourself). Defaults: 100 % / 64 MiB.
    pub fn with_auto_aof_rewrite(mut self, pct: u32, min_size: u64) -> Self {
        self.auto_aof_rewrite_pct = pct;
        self.auto_aof_rewrite_min_size = min_size;
        self
    }

    /// Shard the keyspace into `n` shared-nothing partitions (`hash(key) % n`),
    /// each with its own lock + keyspace + AOF, so concurrent access scales
    /// across cores. `n` clamps to ≥ 1; `1` (default) is the original
    /// single-shard layout. Going from a single-AOF store to `n > 1`
    /// re-shards the existing `aof-0.aof` into `aof-0..aof-{n-1}` on the next
    /// open (the old file is backed up to `aof-0.aof.premigration.<ts>` first).
    /// Pub/sub is process-wide (handled on shard 0), not sharded.
    pub fn with_shards(mut self, n: usize) -> Self {
        self.shards = n.max(1);
        self
    }

    /// Register a push-style metric callback. It receives a [`crate::KevyMetric`] for
    /// each AOF replay (startup) and AOF rewrite (compaction) — wire it to
    /// Prometheus / a log line / a counter. The callback runs synchronously on
    /// the emitting thread (reaper thread for background rewrites), so keep it
    /// fast and non-blocking. Replaces any previously-set sink.
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
    pub fn with_ttl_reaper_manual(mut self) -> Self {
        self.ttl_reaper = TtlReaperMode::Manual;
        self
    }

    /// Override the background reaper interval. Default 100 ms.
    pub fn with_reaper_interval(mut self, iv: Duration) -> Self {
        self.reaper_interval = iv;
        self
    }

    /// Override the snapshot file name inside `data_dir`.
    pub fn with_snapshot_filename(mut self, name: impl Into<String>) -> Self {
        self.snapshot_filename = name.into();
        self
    }

    /// Override the AOF file name inside `data_dir`.
    pub fn with_aof_filename(mut self, name: impl Into<String>) -> Self {
        self.aof_filename = name.into();
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
