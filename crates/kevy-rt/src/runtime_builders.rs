//! Runtime builder methods split out of [`crate::runtime`] so that
//! file stays under the 500-LOC project ceiling. Same `impl Runtime<C>`,
//! split purely by responsibility: construction + boot live in
//! `runtime.rs`; the `with_*` configuration setters live here.

use std::path::PathBuf;

use kevy_persist::Fsync;

use crate::Commands;
use crate::runtime::Runtime;

impl<C: Commands> Runtime<C> {
    /// v3-cluster replication producer side: when `enabled`, each shard
    /// runs a per-shard `ReplicationSource` with `buffer_size` byte
    /// budget. Every applied mutation is pushed to the backlog for
    /// connected replicas to consume. `enabled = false` (default) is
    /// zero hot-path cost — each write checks `Option::is_some()` and
    /// skips. The replication TCP listener / streaming loop arrive in
    /// subsequent v3-cluster tasks (T1.12+); enabling without those
    /// landed means the backlog fills and frames are dropped per the
    /// source's eviction policy, but writes proceed normally.
    #[must_use]
    pub fn with_replication(mut self, enabled: bool, buffer_size: u64) -> Self {
        self.enable_replication = enabled;
        if buffer_size > 0 {
            self.replication_buffer_size = buffer_size;
        }
        self
    }

    /// Bring up a replication listener per shard at
    /// `port_base + shard_id` (per Issue Ledger I2 — mirrors the
    /// cluster listener pattern). Replica clients connect to each
    /// per-shard port to mirror the full keyspace. This is independent
    /// of [`Self::with_replication`]: a primary that runs the producer
    /// backlog without a listener (benchmarks, embed-only) is
    /// supported.
    #[must_use]
    pub fn with_replication_listener(mut self, port_base: u16) -> Self {
        self.replication_port_base = Some(port_base);
        self
    }

    /// Per-shard SlotTable reconnect window in milliseconds — the
    /// grace period a disconnected replica's slot is retained for so
    /// a reconnect within the window can be correlated against its
    /// prior `sent_offset`. Default `60_000` (60 s); pass `0` to drop
    /// slots immediately on disconnect.
    #[must_use]
    pub fn with_replication_reconnect_window(mut self, ms: u32) -> Self {
        self.replication_reconnect_window_ms = ms;
        self
    }

    /// Install per-shard replica inboxes (T1.29). The embedder pre-
    /// constructs `nshards` inbox pairs via
    /// [`crate::replica_inbox_pair`], keeps the senders to hand to
    /// the per-shard replica runner threads, and passes the receivers
    /// here. The order of `receivers` is shard-major: index `i` ↔
    /// shard `i`. Length must equal `nshards`. When this builder
    /// isn't called, no shard has an inbox (the standalone /
    /// primary-only behaviour pre-T1.29).
    #[must_use]
    pub fn with_replica_inboxes(
        mut self,
        receivers: Vec<crate::replica_inbox::ReplicaInboxReceiver>,
    ) -> Self {
        self.replica_inboxes = receivers.into_iter().map(Some).collect();
        self
    }

    /// Enable single-node cluster mode: keys route by Redis-cluster slot
    /// (CRC16 `{hashtag}` & 16383, contiguous even ranges) and every shard
    /// `i` binds a second, deterministic listener at `port_base + i` that
    /// answers wrong-shard keys with `-MOVED` instead of forwarding. The
    /// SO_REUSEPORT listener on the main port keeps today's full
    /// forward-anywhere behaviour for non-cluster clients.
    #[must_use]
    pub fn with_cluster(mut self, port_base: u16) -> Self {
        self.cluster_port_base = Some(port_base);
        self
    }

    /// SLOWLOG tuning (`[slowlog]` config section). Default
    /// `slower_than_micros = -1` (OFF) so the hot path never reads the
    /// clock — every enabled command otherwise pays an `Instant::now()`
    /// pair around dispatch, ~30 ns/op (≈9 % at 3 M ops/s). To match
    /// Redis's 10 ms default, pass `10_000`; `0` records all; `-1`
    /// disables. `max_len` is the per-shard ring cap (default 128).
    #[must_use]
    pub fn with_slowlog(mut self, slower_than_micros: i64, max_len: u32) -> Self {
        self.slowlog_slower_than_micros = slower_than_micros;
        self.slowlog_max_len = max_len;
        self
    }

    /// Reactor tuning knobs (`[advanced]` config section). Defaults
    /// match the pre-v1.4 hardcoded constants. `ring_capacity` is
    /// applied at SPSC ring construction (startup only); the other
    /// three are read at each iteration of the reactor loop, so
    /// values applied here take effect from the next shard.run() call.
    #[must_use]
    pub fn with_advanced(
        mut self,
        spin_limit: u32,
        park_timeout_ms: u32,
        tick_check_every: u32,
        ring_capacity: usize,
    ) -> Self {
        self.spin_limit = spin_limit;
        self.park_timeout_ms = park_timeout_ms;
        self.tick_check_every = tick_check_every;
        self.ring_capacity = ring_capacity;
        self
    }

    /// Set the directory where shards snapshot to / load from. Default: `.`.
    #[must_use]
    pub fn with_data_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.data_dir = dir.into();
        self
    }

    /// Enable/disable the append-only log. Default: enabled.
    #[must_use]
    pub fn with_aof(mut self, on: bool) -> Self {
        self.enable_aof = on;
        self
    }

    /// fsync policy for the AOF. Default `EverySec` matches Redis (lose at
    /// most ~1 s of writes on a crash). `Always` is zero-loss but ~50 %
    /// throughput; `No` defers everything to the OS pagecache.
    #[must_use]
    pub fn with_appendfsync(mut self, fsync: Fsync) -> Self {
        self.appendfsync = fsync;
        self
    }

    /// Auto-trigger BGREWRITEAOF when the live AOF has grown by at least
    /// `pct` percent above its size at the previous rewrite, AND is at
    /// least `min_size` bytes. `pct=0` disables auto-rewrite (clients can
    /// still run BGREWRITEAOF manually). Defaults: 100 % / 64 MiB.
    #[must_use]
    pub fn with_auto_aof_rewrite(mut self, pct: u32, min_size: u64) -> Self {
        self.auto_aof_rewrite_pct = pct;
        self.auto_aof_rewrite_min_size = min_size;
        self
    }
}
