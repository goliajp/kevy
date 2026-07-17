//! The public entry point: configure and run the thread-per-core server.

use crate::Commands;
use kevy_persist::Fsync;
use std::path::PathBuf;

/// Default slots in each per-core-pair SPSC ring. A full ring spills
/// to a local backlog (see [`Shard`]), so this only bounds the
/// lock-free fast path, not capacity. Overridable via the
/// `[advanced] ring_capacity` config field threaded through
/// [`Runtime::with_advanced`].
const DEFAULT_RING_CAPACITY: usize = 1024;

/// The public entry point: configure and run the thread-per-core server.
pub struct Runtime<C: Commands> {
    pub(crate) ip: [u8; 4],
    pub(crate) port: u16,
    pub(crate) nshards: usize,
    pub(crate) commands: C,
    /// Directory for per-shard snapshot files (`dump-<id>.rdb`) and AOF logs.
    pub(crate) data_dir: PathBuf,
    /// Whether the append-only log is enabled.
    pub(crate) enable_aof: bool,
    /// fsync policy for the AOF. Default `EverySec` matches Redis.
    pub(crate) appendfsync: Fsync,
    /// auto-trigger BGREWRITEAOF when AOF grew this many % above the size
    /// at the previous rewrite. `0` disables. Default `100` (matches Redis).
    pub(crate) auto_aof_rewrite_pct: u32,
    pub(crate) auto_aof_rewrite_bytes: u64,
    pub(crate) auto_aof_rewrite_interval_secs: u64,
    /// Floor below which auto-rewrite is skipped. Default `64 MiB`.
    pub(crate) auto_aof_rewrite_min_size: u64,
    /// Reactor SPSC ring slot count. See [`DEFAULT_RING_CAPACITY`].
    pub(crate) ring_capacity: usize,
    /// Reactor busy-poll iter limit before parking. Stored as `u32`
    /// for the per-shard counter; the [`Shard`] field carries it
    /// forward into the loop.
    pub(crate) spin_limit: u32,
    /// `Some(N)` = only shards `0..N` arm accept SQE. `None`
    /// = every shard accepts (the default; byte-identical to the
    /// pre-flag behaviour).
    pub(crate) accept_shards: Option<usize>,
    /// Total cap on active client conns. `0` = unlimited.
    pub(crate) max_clients: usize,
    /// Reactor blocking-wait timeout in ms when parked.
    pub(crate) park_timeout_ms: u32,
    /// Wall-clock-read throttle for the tick check (TTL reaper / live
    /// config refresh / auto-AOF-rewrite).
    pub(crate) tick_check_every: u32,
    /// `[slowlog].slower_than_micros`. Default: `-1` (OFF — zero
    /// hot-path cost: every command would otherwise pay an
    /// `Instant::now()` pair around dispatch). Set to `10_000` to match
    /// Redis's default 10 ms threshold; see [`Self::with_slowlog`] /
    /// `CONFIG SET slowlog-log-slower-than 10000`.
    pub(crate) slowlog_slower_than_micros: i64,
    /// `[slowlog].max_len`. Per-shard cap.
    pub(crate) slowlog_max_len: u32,
    /// Single-node cluster mode: slot-based key routing (CRC16 `{hashtag}`
    /// → contiguous ranges) + one deterministic extra listener per shard at
    /// `cluster_port_base + id`. `None` = off (default, zero change).
    pub(crate) cluster_port_base: Option<u16>,
    /// Replication: when `true`, each shard runs a
    /// `ReplicationSource` with `replication_buffer_size` byte budget;
    /// every applied mutation is pushed to the backlog. This wires
    /// only the producer side — the TCP listener + streaming loop are
    /// gated separately by `replication_port_base`. Default `false`.
    pub(crate) enable_replication: bool,
    /// FEED.* consumer surface. When set, every shard keeps a
    /// backlog (even with no replicas) and persists the (generation,
    /// offset) cursor via the feed sidecars.
    pub(crate) feed_enabled: bool,
    /// Per-shard backlog byte budget for the feed (`[feed]
    /// feed_buffer_size`); the effective budget is
    /// `max(replication_buffer_size, feed_buffer_size)` when both
    /// features are on (one backlog, two readers).
    pub(crate) feed_buffer_size: u64,
    /// Per-shard backlog byte budget when `enable_replication` is set.
    /// Fed from `[replication] replication_buffer_size`. Default
    /// `256 MiB` (matches the kevy-config default).
    pub(crate) replication_buffer_size: u64,
    /// v3-cluster replication listener: shard `i` binds at
    /// `replication_port_base + i` (mirrors cluster listener pattern;
    /// per Issue Ledger I2). `None` = no listener (producer side runs
    /// without a network surface, backlog accumulates and evicts —
    /// useful for benchmarks). Default `None`.
    pub(crate) replication_port_base: Option<u16>,
    /// Per-shard SlotTable reconnect-window in ms. After a
    /// streaming replica disconnects, its `(replica_id, sent_offset)`
    /// is recorded in the shard's `slots` map; slots past this age
    /// are reaped on the next shard tick. Default `60_000` (60 s)
    /// matches the kevy-config default.
    pub(crate) replication_reconnect_window_ms: u32,
    /// Per-shard replica inboxes installed by
    /// [`Self::with_replica_inboxes`]. Each entry is consumed
    /// (via `Option::take`) when its shard is constructed, so the
    /// receiver flows from this Vec to the matching `Shard.replica_inbox`.
    /// Empty when no replica mode is configured.
    pub(crate) replica_inboxes: Vec<Option<crate::replica_inbox::ReplicaInboxReceiver>>,
    /// UDS: when `Some(path)`, ALSO bind a Unix-domain stream
    /// listener at `path` on shard 0 (single global socket, like valkey's
    /// `unixsocket` config). Lets benches/local clients skip TCP loopback
    /// overhead. TCP listener stays bound regardless.
    #[allow(dead_code)] // consumed during run() via take-into-Shard
    pub(crate) unix_socket_path: Option<PathBuf>,
}

impl<C: Commands> Runtime<C> {
    /// Start configuring a runtime for `commands`. `Runtime` is its own
    /// builder: chain [`Self::bind`] / [`Self::shards`] / the `with_*`
    /// setters, then call `run`. Defaults: bind `127.0.0.1:6004`, one
    /// shard, AOF on (`EverySec`), data dir `"."`.
    #[must_use]
    pub fn builder(commands: C) -> Self {
        Runtime {
            ip: [127, 0, 0, 1],
            port: 6004,
            nshards: 1,
            commands,
            data_dir: PathBuf::from("."),
            enable_aof: true,
            appendfsync: Fsync::EverySec,
            auto_aof_rewrite_pct: 100,
            auto_aof_rewrite_bytes: 0,
            auto_aof_rewrite_interval_secs: 0,
            auto_aof_rewrite_min_size: 64 * 1024 * 1024,
            ring_capacity: DEFAULT_RING_CAPACITY,
            spin_limit: 256,
            accept_shards: None,
            max_clients: 10_000,
            park_timeout_ms: 50,
            tick_check_every: 256,
            slowlog_slower_than_micros: -1,
            slowlog_max_len: 128,
            cluster_port_base: None,
            enable_replication: false,
            feed_enabled: false,
            feed_buffer_size: 64 * 1024 * 1024,
            replica_inboxes: Vec::new(),
            replication_buffer_size: 256 * 1024 * 1024,
            replication_port_base: None,
            replication_reconnect_window_ms: 60_000,
            unix_socket_path: None,
        }
    }

    /// Listen address for the client TCP listener (every shard binds it
    /// via SO_REUSEPORT). Default `127.0.0.1:6004`.
    #[must_use]
    pub fn bind(mut self, ip: [u8; 4], port: u16) -> Self {
        self.ip = ip;
        self.port = port;
        self
    }

    /// Shard (reactor thread) count. Clamped to at least 1. Default 1.
    #[must_use]
    pub fn shards(mut self, n: usize) -> Self {
        self.nshards = n.max(1);
        self
    }

    /// Spawn one thread per shard and run until `stop` is set.
    /// UDS: also bind a Unix-domain stream listener at `path`. Lets
    /// local clients (and benchmarks) skip the TCP loopback round-trip.
    /// Bound on shard 0 only (no SO_REUSEPORT for AF_UNIX, single global
    /// socket like valkey's `unixsocket` config). TCP listener stays
    /// bound at the configured `port` regardless.
    #[must_use]
    pub fn with_unix_socket(mut self, path: PathBuf) -> Self {
        self.unix_socket_path = Some(path);
        self
    }

    // `run` (and its per-stage build helpers) lives in
    // [`crate::runtime_run`] — same `impl<C: Commands> Runtime<C>`,
    // split out so this file stays under the 500-LOC house rule.
}
