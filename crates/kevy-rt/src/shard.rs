//! One shard = one core: the reactor (kqueue/epoll) plus the keyspace it owns.
//!
//! This module is the *transport* half: accepting connections, reading/parsing
//! requests, draining the cross-core inbound rings, and flushing replies in seq
//! order. The command *semantics* (routing, execution, result reduction) live in
//! [`crate::exec`], which adds a second `impl Shard` block. The [`Shard::run`]
//! loop drives socket readiness and the inbound rings until `stop` is set.
//!
//! Cross-core transport is a lock-free SPSC ring per ordered core-pair
//! ([`kevy_ring`]). When a peer's ring is momentarily full, the message spills to
//! a local per-target `backlog`; the loop keeps draining its own inbound and
//! flushing backlogs every iteration, so no shard ever blocks waiting on a peer —
//! that is what keeps the all-to-all mesh deadlock-free.

use crate::Commands;
use crate::NotificationFlags;
use crate::blocked::BlockedClients;
use crate::conn::Conn;
use crate::message::{Inbound, PubMsg, PubSubPatternReg, PubSubReg, ReqBatch};
use kevy_map::KevyMap;
use kevy_persist::Aof;
use kevy_ring::{Consumer, Producer};
use kevy_store::Store;
use kevy_sys::{Event, Poller, Socket, Waker};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::time::Instant;

pub(crate) use crate::cache_padded::CachePadded;

pub(crate) struct Shard<C: Commands> {
    /// Outstanding cross-shard requests this shard has forwarded and
    /// not yet folded replies for. While non-zero, replies are
    /// certainly inbound within ~one cross-shard RTT — the idle
    /// ladder stays in the spin rung instead of paying a kernel
    /// sleep/wake transition per reply batch (the fix for a measured
    /// multi-shard owner-starvation regression; see the legacy8sh
    /// owner-starvation PERF-DECOMP note in bench/).
    pub(crate) xshard_inflight: u64,
    pub(crate) id: usize,
    pub(crate) nshards: usize,
    /// Cluster mode (`Some` = on): switches key→shard routing from KevyHash
    /// to Redis-cluster slots (CRC16 `{hashtag}` → contiguous ranges) and
    /// carries the advertised per-shard addressing for `-MOVED` replies.
    /// Startup-time property (recorded in `shards.meta`), never flipped
    /// live. See [`crate::reduce::shard_of`].
    pub(crate) cluster: Option<crate::cluster::ClusterTopo>,
    /// The per-shard deterministic listener (`cluster.port_base + id`),
    /// `Some` iff cluster mode is on. Conns accepted here are marked
    /// [`Conn::cluster`] and get `-MOVED` instead of forwarding.
    pub(crate) cluster_listener: Option<Socket>,
    /// UDS: optional Unix-domain stream listener. Only shard 0 ever
    /// holds it (no SO_REUSEPORT for AF_UNIX, so a single global socket
    /// like valkey's `unixsocket` config). The reactor accepts on it
    /// alongside the TCP listener. `None` on all other shards + when
    /// `Runtime::with_unix_socket` wasn't called.
    #[allow(dead_code)] // used by uring_reactor and shard_run accept paths
    pub(crate) unix_listener: Option<Socket>,
    pub(crate) store: Store,
    pub(crate) commands: C,
    pub(crate) poller: Poller,
    /// `None` on off-accept-set shards; they don't bind the listener so SO_REUSEPORT redirects.
    pub(crate) listener: Option<Socket>,
    pub(crate) waker: Arc<Waker>,
    /// Inbound SPSC ring from each peer shard (index = source id; `self` = None).
    pub(crate) inboxes: Vec<Option<Consumer<Inbound>>>,
    /// Outbound SPSC ring to each peer shard (index = target id; `self` = None).
    pub(crate) outboxes: Vec<Option<Producer<Inbound>>>,
    /// Per-target overflow queue: messages that didn't fit a full outbound ring,
    /// re-pushed (in order) by `flush_backlog` once the peer drains.
    pub(crate) backlog: Vec<VecDeque<Inbound>>,
    pub(crate) wakers: Vec<Arc<Waker>>,
    // Fx-hashed: these are looked up per command (`conns` twice — start_command
    // + fold) and per event; std's SipHash on the u64/i32 keys profiled at ~17%
    // of single-shard CPU, the dominant non-command-CPU cost.
    pub(crate) conns: KevyMap<u64, Conn>,
    /// Per-iter "needs arm work" queue
    /// for the io_uring reactor. Populated by:
    ///   - accept handler (new conn, needs recv arm)
    ///   - `uring_on_recv` (produced output via dispatch / recv
    ///     terminated and needs re-arm)
    ///   - `uring_on_write` (chunked-writev tail still queued)
    ///   - `drain_inbound` (folded reply pushed to `conn.output`)
    ///   - pub/sub `deliver_publish*` paths (already push `self.dirty`;
    ///     `uring_arm_conns` drains those into `arm_pending`)
    ///   - block / xshard reply paths (same dirty-list reuse)
    ///   - `uring_mark_closing` (closing conn needs visit until reap)
    ///
    /// Drained by `uring_arm_conns` each iter. Replaces the O(N=conns)
    /// `active_uring_conns` Vec scan, which at
    /// c=10k was 50 µs/iter raw — now O(active) per iter.
    /// Dedup via `UringConn::arm_queued`.
    #[allow(dead_code)] // io_uring path only — epoll reactor doesn't use it
    pub(crate) arm_pending: Vec<u64>,
    /// Ready-set for
    /// `uring_reap_closed`. Mirror of `arm_pending`, applied to the reap
    /// path. Populated by `uring_mark_closing` + QUIT dispatch sites
    /// (`exec_dispatch::try_inline_local` + `start_single_at_seq` —
    /// every place that flips `conn.closing = true`). Drained via
    /// `mem::take` on each reap pass.
    ///
    /// **Why this exists** (perf-record-dwarf, c=10 000 -P 1 SET, 10 s
    /// sustained): the old `uring_reap_closed` body
    /// `io.iter().filter(...).map(|(cid,_)| (cid, self.conns.get(cid)))
    /// .collect::<Vec<u64>>()` was 36.74 % of CPU at c=10 000 — pure
    /// O(N) scan + per-entry second hash lookup into `self.conns`. An
    /// earlier change split this for `arm_conns` but left
    /// `uring_reap_closed` untouched (hence bench-neutral); this field
    /// finishes the ready-set shape across BOTH callsites.
    ///
    /// Dedup: a conn pushed twice ends up reaped on the first hit (the
    /// second hit's `self.conns.get(cid)` returns None and the filter
    /// short-circuits).
    #[allow(dead_code)] // io_uring path only
    pub(crate) closing_uring_conns: Vec<u64>,
    pub(crate) fd_to_conn: KevyMap<i32, u64>,
    /// Next conn id to assign. Starts at `shard_id + 1` and strides by
    /// [`Self::conn_id_step`], so ids are unique across every shard of
    /// the instance (the CLIENT ID / CLIENT KILL ID contract).
    pub(crate) next_conn_id: u64,
    /// Conn-id stride == the instance's shard count.
    pub(crate) conn_id_step: u64,
    pub(crate) events: Vec<Event>,
    pub(crate) read_buf: Vec<u8>,
    /// Bitmap of targets that received a message this iteration but
    /// haven't been woken yet. Wakeups are coalesced: each target is woken
    /// at most once per loop, not once per message (one pipe-write
    /// syscall instead of N). A perf profile showed `flush_wakes`
    /// at 2.6% of -c1 CPU on its fast-path early-return alone — the
    /// `Vec<bool>::iter().any(|&w| w)` was N byte loads per iter. The u64
    /// folds it to a single `!= 0` load. Limit: `nshards ≤ 64`, shared
    /// with `inbound_dirty`.
    pub(crate) pending_wakes: u64,
    /// Bitmap of `backlog[dst]`'s that are non-empty. Maintained by
    /// `send_to` (set the bit when we spill) and `flush_backlog` (clear
    /// the bit when we drain a target empty). Same motivation as
    /// `pending_wakes`: the early-return path was `backlog.iter().all(
    /// VecDeque::is_empty)`, N struct accesses per iter; the u64 is one
    /// load.
    pub(crate) backlog_nonempty: u64,
    /// Bitmap of `request_batch[dst]`'s that are non-empty. Same shape
    /// as `backlog_nonempty`: set by the dispatch sites that push into
    /// `request_batch[s]`, cleared in `flush_requests` when the batch
    /// is drained. Removes the per-iter
    /// `request_batch.iter().all(Vec::is_empty)` scan from the
    /// `run_uring` main-loop self-time hot block.
    pub(crate) request_batch_nonempty: u64,
    /// Bitmap of `publish_batch[dst]`'s that are non-empty. Mirror of
    /// `request_batch_nonempty` for the pub/sub fan-out path.
    pub(crate) publish_batch_nonempty: u64,
    /// Per-shard "is this core parked (blocking) right now?" flags. A sender only
    /// needs a syscall wakeup for a parked peer; a spinning peer sees the message
    /// on its next poll. Indexed by shard id; `parked[self.id]` is our own.
    pub(crate) parked: Vec<Arc<CachePadded<AtomicBool>>>,
    /// Per-shard inbox-dirty bitmaps. `inbound_dirty[me]` is owned by shard
    /// `me`: a sender from shard `src` calls `inbound_dirty[me].fetch_or(1
    /// << src, Release)` after pushing a message into `inboxes[src]`, so
    /// `drain_inbound_core` can `swap(0, AcqRel)` and short-circuit when no
    /// peer has written. A perf profile showed
    /// `uring_drain_inbound` at 17.4% of -c1 CPU even with no cross-shard
    /// traffic — the per-iteration scan of N empty ring-queue tails was
    /// the cost; this flag collapses it to one atomic load. Limit:
    /// `nshards ≤ 64` (one bit per peer in a single `u64`); enforced by
    /// `debug_assert` in [`Self::run`].
    pub(crate) inbound_dirty: Vec<Arc<CachePadded<AtomicU64>>>,
    pub(crate) data_dir: PathBuf,
    /// `None` disables the append-only log (e.g. pure in-memory benchmarking).
    pub(crate) aof: Option<Aof>,
    /// Two-phase rewrite handoff state. `Some` between the worker's
    /// image-spill completing and the final swap (or a divergence
    /// abort). See `persist_rewrite.rs` for the protocol.
    pub(crate) rewrite_handoff: Option<crate::persist_rewrite::RewriteHandoff>,
    /// `(when, aof size)` at the last auto-rewrite rate sample — the
    /// begin-gate's memory (see `maybe_auto_rewrite_aof`).
    pub(crate) rewrite_rate_mark: Option<(std::time::Instant, u64)>,
    /// Consecutive due-ticks below the begin-gate's rate threshold —
    /// the gate's hysteresis (a storm's momentary lull must not admit
    /// a giant postponed attempt).
    pub(crate) rewrite_calm_ticks: u32,
    /// io_uring AOF offload state (RFC v3-aof-offload S1); dormant
    /// unless the reactor's setup opts in.
    #[cfg(target_os = "linux")]
    pub(crate) aof_offload: crate::uring_aof::AofOffload,
    /// S3: the epoll/kqueue reactors' AOF writer lane; dormant unless
    /// that reactor's setup opts in (uring shards never enable it).
    pub(crate) aof_lane: crate::aof_writer::AofWriterLane,
    /// A live-config fsync-policy switch waiting for the offload
    /// driver's in-flight appends to drain: `set_fsync`'s Always
    /// upgrade flushes the queue through the OWNER handle, which must
    /// not interleave with writes the driver already holds. Applied
    /// (and cleared) by `try_apply_fsync_policy` on the tick.
    pub(crate) pending_fsync_policy: Option<kevy_persist::Fsync>,
    /// S2/S3 Always reply gate, cross-shard half: ResponseBatches
    /// whose forwarded writes are queued but not yet fsync-proven.
    /// Each entry is (record watermark, origin shard, batch); flushed
    /// by `flush_held_responses` once the durable watermark passes.
    pub(crate) held_responses: Vec<(u64, usize, crate::message::Inbound)>,
    /// Per-shard replication backlog. `None` when `[replication] role`
    /// is `standalone` (default — zero hot-path cost: every write checks
    /// `replicate.is_some()` and skips). `Some` when `role = "primary"`
    /// — every applied mutation is pushed to the backlog for connected
    /// replicas to consume.
    /// Replication backlog + feed cursor. Populated when
    /// replication is on OR `feed_enabled` (FEED.* consumers need the
    /// backlog even with no replicas); `None` = both features off.
    pub(crate) replicate: Option<kevy_replicate::feed::FeedSource>,
    /// Per-shard replication listener (per Issue Ledger I2): shard `i`
    /// binds at `replication_port_base + i`. `Some` only when the
    /// runtime was built with [`crate::Runtime::with_replication_listener`].
    /// Accepted connections enter the [`crate::replication::ReplicaConn`]
    /// state machine — handshake → live frame streaming.
    pub(crate) replication_listener: Option<Socket>,
    /// Active replica connections (handshake-pending or streaming).
    /// Vec rather than KevyMap — N < 16 in practice, linear scan
    /// beats hashing at that size.
    pub(crate) replicas: Vec<crate::replication::ReplicaConn>,
    /// Recently-disconnected replica slots. A replica that
    /// drops within `replication_reconnect_window_ms` is correlated
    /// against its prior `sent_offset`; expired by the shard tick.
    pub(crate) slots: kevy_replicate::slot::SlotTable,
    /// Reconnect window for [`Self::slots`] in ms, fed from
    /// `[replication] reconnect_window_ms`.
    pub(crate) replication_reconnect_window_ms: u32,
    /// Monotonic time origin for slot timestamps. `SlotTable` takes
    /// `u64` ns; we derive them as
    /// `Instant::now().duration_since(replication_epoch).as_nanos()`.
    pub(crate) replication_epoch: Instant,
    /// Per-shard replica inbox — `Some` when this server runs as a
    /// replica. The replica runner thread sends decoded
    /// snapshot/frame events into this receiver; the reactor drains
    /// it once per tick and applies via `kevy::dispatch` (under
    /// [`crate::ReplicatedApplyGuard`]). `None` when the shard is a
    /// primary or standalone.
    pub(crate) replica_inbox: Option<crate::replica_inbox::ReplicaInboxReceiver>,
    /// Accumulating snapshot bytes for an in-progress
    /// [`crate::ReplicaApply::SnapshotChunk`] sequence. Reset on each
    /// `SnapshotBegin`; consumed on `SnapshotEnd` (`load_snapshot_from`
    /// into `self.store`). Empty when no snapshot is in flight.
    pub(crate) replica_snapshot_buf: Vec<u8>,
    /// This shard's replication-apply position: the offset
    /// the NEXT applied frame should carry (last applied + 1; a
    /// snapshot load jumps it to the ship's `ack_offset`). Advanced on
    /// the reactor thread in [`crate::replication_apply`], so a
    /// `REPL.WAIT` answered against it is ordered BEFORE any
    /// subsequent read on this shard — the read-your-writes truth
    /// (the runner's ACK cursor counts frames merely *enqueued* into
    /// the inbox and would race a following GET). 0 when this shard
    /// never applied anything.
    pub(crate) replica_applied_next: u64,
    /// Parked WAIT / REPL.WAIT participants on this shard
    /// (see [`crate::exec_replwait`]). Empty in steady state; every
    /// wake/tick hook short-circuits on `is_empty()`.
    pub(crate) repl_waiters: Vec<crate::exec_replwait::ReplWaiter>,
    /// Last [`LiveRuntimeConfig::promotion_epoch`] this
    /// shard applied. `None` until the first tick: the first observed
    /// value is RECORDED but not acted on, so a process-global counter
    /// left over from an earlier serve session (in-process restarts,
    /// test binaries) can't fire a spurious generation bump at boot.
    pub(crate) seen_promotion_epoch: Option<u64>,
    /// `auto_aof_rewrite_percentage`: trigger BGREWRITEAOF when the live
    /// AOF is at least this percent larger than at the previous rewrite.
    /// `0` disables auto-rewrite.
    pub(crate) auto_aof_rewrite_pct: u32,
    /// `auto_aof_rewrite_min_size`: never auto-rewrite an AOF smaller than
    /// this many bytes (prevents thrash during startup / on tiny data).
    pub(crate) auto_aof_rewrite_min_size: u64,
    /// `auto_aof_rewrite_bytes`: absolute-size trigger (0 = rule off).
    pub(crate) auto_aof_rewrite_bytes: u64,
    /// `auto_aof_rewrite_interval_secs`: staleness trigger (0 = rule off).
    pub(crate) auto_aof_rewrite_interval_secs: u64,
    /// Best-effort boot replay (see `Runtime::with_replay_resync`).
    pub(crate) replay_resync: bool,
    /// Connections a PUBLISH appended output to this iteration; the reactor
    /// flushes them (epoll via `flush_conn`, io_uring via its arm/write loop).
    pub(crate) dirty: Vec<u64>,
    /// Shared pub/sub channel registry (see [`PubSubReg`]).
    pub(crate) pubsub: PubSubReg,
    /// Shared pub/sub pattern registry (see [`PubSubPatternReg`]).
    /// Empty in steady state; PUBLISH skips the walk when so.
    pub(crate) pubsub_patterns: PubSubPatternReg,
    /// This shard's local pattern → conn ids table. Mirrors `pubsub`'s
    /// channel-table role for the channel-precise path. Each
    /// `PSUBSCRIBE` adds an entry; each delivered `PUBLISH` runs
    /// `glob_match` against every key (only when the map is non-empty —
    /// the steady-state O(1) `is_empty()` short-circuit keeps the
    /// channel-only PUBLISH hot path untouched).
    pub(crate) psub_local: HashMap<Vec<u8>, Vec<u64>>,
    /// Per-channel local subscriber index — mirrors
    /// redis `pubsub.c:479-485`. Replaces the O(total_conns) global
    /// scan in `deliver_publish` with O(1) `HashMap::get`. Maintained
    /// on SUBSCRIBE/UNSUBSCRIBE + close_conn; entry removed at zero.
    pub(crate) subs_by_channel: HashMap<Vec<u8>, Vec<u64>>,
    /// Per-target-shard accumulated pub/sub deliveries, flushed once per loop
    /// (`flush_publish`) so a PUBLISH flood batches into one send per shard.
    pub(crate) publish_batch: Vec<Vec<PubMsg>>,
    /// Per-owning-shard accumulated single-key dispatches, flushed once per loop
    /// (`flush_requests`) so a -c50 flood costs one cross-core send per shard,
    /// not one per command — amortizing the ring/fold tax that drags many
    /// shards below single-shard throughput.
    pub(crate) request_batch: Vec<ReqBatch>,
    /// Per-shard cached `notify_keyspace_events` flags — hot-reloaded
    /// off the [`crate::Commands::live_runtime_config`] tick. Empty
    /// (default) = OFF: every write checks `notify_flags.is_empty()`
    /// and skips the publish hot-path. `Copy` so the per-cmd check
    /// fits in a register pair.
    pub(crate) notify_flags: NotificationFlags,
    /// Iterations the reactor busy-polls before parking. Threaded in
    /// from [`crate::Runtime::with_advanced`]; replaces the old
    /// `SPIN_LIMIT` const so embedders can tune wake-up vs idle CPU.
    pub(crate) spin_limit: u32,
    /// Bounded blocking-wait timeout (ms) when parked. Acts as a
    /// safety backstop against missed cross-core wakes. Replaces the
    /// old `PARK_TIMEOUT_MS` const.
    pub(crate) park_timeout_ms: i32,
    /// Reactor loop iterations between wall-clock reads for the tick
    /// check. Replaces the old `TICK_CHECK_EVERY` const.
    pub(crate) tick_check_every: u32,
    /// `false` = compute-only shard (no accept SQE).
    pub(crate) arms_accept: bool,
    /// Per-shard cap (`max_clients / nshards`). `0` = unlimited.
    pub(crate) max_clients_per_shard: usize,
    /// [`crate::CLIENT_INPUT_HARD_LIMIT`], after the debug-env override.
    pub(crate) input_hard_limit: usize,
    /// Accumulator for `rejected_connections` (INFO clients).
    pub(crate) rejected_connections: u64,
    /// SLOWLOG ring + threshold (see [`crate::exec_slowlog::SlowlogState`]).
    /// Hot-reload via `apply_live_runtime_config` when the embedder
    /// returns `Some` in `LiveRuntimeConfig::slowlog_*`.
    pub(crate) slowlog: crate::exec_slowlog::SlowlogState,
    /// Per-shard blocked-client registry (see [`crate::blocked`]). The
    /// in-shard fast path for a single key on this shard: `BLPOP` /
    /// `BRPOP` / `XREAD BLOCK` / `XREADGROUP BLOCK`. Empty in steady
    /// state, so the wake / tick hot paths short-circuit on `is_empty()`.
    pub(crate) blocked: BlockedClients,
    /// Origin-side records for conns blocked across shards (a single
    /// remote key or any multi-key form). This shard is the arbiter for
    /// each. Empty in steady state. See [`crate::block_xshard`].
    pub(crate) origin_blocks: HashMap<u64, crate::block_xshard::OriginBlock>,
    /// Target-side cross-shard waiters: (possibly remote) conns blocked on
    /// keys this shard owns. Kept separate from `blocked` so the hot
    /// single-key-local path is untouched. Empty in steady state.
    pub(crate) xwaiters: crate::block_xshard::XShardWaiters,
    /// Cross-shard block serves whose escrow release is waiting on the
    /// reply's write result: `conn -> target_shard`. A serve reply is
    /// buffered but the escrow is NOT released until the write succeeds
    /// (drained → release) or the conn is torn down (write failed / FIN →
    /// restore). This is what ties escrow release to actual delivery rather
    /// than to a point-in-time liveness guess. Empty in steady state.
    pub(crate) serve_confirm: HashMap<u64, usize>,
    /// Reused staging buffer for forwarded-dispatch replies
    /// (`Shard::run_dispatch`): `dispatch_into` writes here, then ≤30 B replies
    /// copy into a stack-inline [`crate::message::SmallReply`] — zero
    /// allocator traffic for the +OK / :N / small-GET steady state.
    pub(crate) reply_scratch: Vec<u8>,
    /// Recycled `Argv`s for the cross-shard forward path: the forward
    /// sites fill from here (no malloc in steady state), and the
    /// `ResponseBatch` handler drops each returned husk back in. See
    /// [`kevy_resp::ArgvPool`] for the ownership cycle.
    pub(crate) argv_pool: kevy_resp::ArgvPool,
    /// Background persister (BGSAVE / BGREWRITEAOF / auto-rewrite); the
    /// thread spawns lazily on the first job. Declared last on purpose:
    /// appending here leaves every pre-existing field's offset unchanged
    /// (adding it mid-struct shifted the forwarded-path hot fields —
    /// `request_batch` / `argv_pool` / `reply_scratch` — and cost ~4% on
    /// the 8sh forward-heavy GET angle). See [`crate::persist_worker`].
    pub(crate) persist: crate::persist_worker::PersistWorker,
}

// `SPIN_LIMIT` / `PARK_TIMEOUT_MS` / `TICK_CHECK_EVERY` are per-shard
// fields (`Shard.spin_limit` / `park_timeout_ms` / `tick_check_every`)
// — wired through `[advanced]` config + `Runtime::with_advanced`.
// Defaults (`256` / `50ms` / `256`) match the original hardcoded
// values, so existing benchmark numbers translate one-to-one.

// The reactor loop ([`Shard::run`]) + the shared path helpers
// (`shard_of` / `snapshot_path` / `aof_path`) live in
// [`crate::shard_run`] — same `impl<C: Commands> Shard<C>`, split out
// so this file stays under the 500-LOC house rule.
