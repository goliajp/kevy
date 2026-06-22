//! The public entry point: configure and run the thread-per-core server.

use crate::Commands;
use crate::message::{Inbound, PubSubPatternReg, PubSubReg};
use crate::shard::Shard;
use kevy_map::KevyMap;
use kevy_persist::{Aof, Fsync};
use kevy_ring::{Consumer, Producer};
use kevy_store::Store;
use kevy_sys::{Poller, Waker, tcp_listen_reuseport, waker};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, RwLock};

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
    /// Floor below which auto-rewrite is skipped. Default `64 MiB`.
    pub(crate) auto_aof_rewrite_min_size: u64,
    /// Reactor SPSC ring slot count. See [`DEFAULT_RING_CAPACITY`].
    pub(crate) ring_capacity: usize,
    /// Reactor busy-poll iter limit before parking. Stored as `u32`
    /// for the per-shard counter; the [`Shard`] field carries it
    /// forward into the loop.
    pub(crate) spin_limit: u32,
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
    /// v3-cluster replication: when `true`, each shard runs a
    /// `ReplicationSource` with `replication_buffer_size` byte budget;
    /// every applied mutation is pushed to the backlog. The TCP
    /// listener + streaming loop arrive in subsequent tasks (T1.12+);
    /// this batch only wires the producer side. Default `false`.
    pub(crate) enable_replication: bool,
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
    /// Per-shard SlotTable reconnect-window in ms (T1.15). After a
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
}

impl<C: Commands> Runtime<C> {
    #[must_use]
    pub fn new(ip: [u8; 4], port: u16, nshards: usize, commands: C) -> Self {
        Runtime {
            ip,
            port,
            nshards: nshards.max(1),
            commands,
            data_dir: PathBuf::from("."),
            enable_aof: true,
            appendfsync: Fsync::EverySec,
            auto_aof_rewrite_pct: 100,
            auto_aof_rewrite_min_size: 64 * 1024 * 1024,
            ring_capacity: DEFAULT_RING_CAPACITY,
            spin_limit: 256,
            park_timeout_ms: 50,
            tick_check_every: 256,
            slowlog_slower_than_micros: -1,
            slowlog_max_len: 128,
            cluster_port_base: None,
            enable_replication: false,
            replica_inboxes: Vec::new(),
            replication_buffer_size: 256 * 1024 * 1024,
            replication_port_base: None,
            replication_reconnect_window_ms: 60_000,
        }
    }


    /// Spawn one thread per shard and run until `stop` is set.
    pub fn run(mut self, stop: Arc<AtomicBool>) -> io::Result<()> {
        let n = self.nshards;

        // v1.25 A.3 (B2: single global bio thread, per
        // `bench/V125-DECISIONS-PENDING.md`). Spawn BEFORE shards so
        // every shard's first overwrite already has a live consumer.
        // The held `bio_send` is moved into the shard loop below
        // (`store.set_bio_drop_sender`); shutdown ordering is:
        //   1. shards return → their `Store`s drop → their cloned
        //      Sender halves drop
        //   2. this fn's local `bio_send` is dropped here at end of
        //      scope → channel closes → bio thread's `recv()` returns
        //      Err → bio thread exits
        //   3. `bio_handle.join()` blocks until that exit so a final
        //      large free isn't truncated by process tear-down
        // (`madvise` returning the page to the kernel still needs the
        // process alive). See `crate::bio` for the full rationale.
        let (bio_send, bio_handle) = crate::bio::spawn();

        // Cluster binds shard `i` at `port_base + i`; reject a range that
        // overflows u16 up front (loud) instead of wrapping a listener onto
        // a low/privileged port while CLUSTER SLOTS advertises 65536+.
        if let Some(base) = self.cluster_port_base
            && base as usize + n > u16::MAX as usize + 1
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "cluster port range {base}..={} exceeds 65535 ({n} shards)",
                    base as usize + n - 1
                ),
            ));
        }

        // Same overflow check for the replication port range
        // (`base + 0 .. base + n`). See Issue Ledger I2 for the
        // per-shard listener decision.
        if let Some(base) = self.replication_port_base
            && base as usize + n > u16::MAX as usize + 1
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "replication port range {base}..={} exceeds 65535 ({n} shards)",
                    base as usize + n - 1
                ),
            ));
        }

        // One lock-free SPSC ring per ordered core-pair (i→j): the producer goes
        // to shard i's outbox[j], the consumer to shard j's inbox[i]. There is no
        // self-ring — a shard runs its own commands inline, never over a ring.
        let mut outboxes: Vec<Vec<Option<Producer<Inbound>>>> =
            (0..n).map(|_| (0..n).map(|_| None).collect()).collect();
        let mut inboxes: Vec<Vec<Option<Consumer<Inbound>>>> =
            (0..n).map(|_| (0..n).map(|_| None).collect()).collect();
        for i in 0..n {
            for j in 0..n {
                if i == j {
                    continue;
                }
                let (p, c) = kevy_ring::ring::<Inbound>(self.ring_capacity);
                outboxes[i][j] = Some(p);
                inboxes[j][i] = Some(c);
            }
        }

        let mut wakers: Vec<Arc<Waker>> = Vec::with_capacity(n);
        for _ in 0..n {
            wakers.push(Arc::new(waker()?));
        }
        let parked: Vec<Arc<crate::shard::CachePadded<AtomicBool>>> = (0..n)
            .map(|_| Arc::new(crate::shard::CachePadded::new(AtomicBool::new(false))))
            .collect();
        // Per-shard inbox-dirty bitmaps (one u64 bit per peer src).
        // Senders OR a bit on the target's dirty word; the target's
        // `drain_inbound_core` swaps and short-circuits when 0.
        assert!(
            n <= 64,
            "kevy-rt: shard count {n} exceeds 64 — inbound_dirty bitmap holds one bit per peer in a u64. Reduce --threads or extend to a multi-word bitmap.",
        );
        // A2 (2026-06-20): pad each Arc<AtomicU64> to a full 64-byte cache
        // line. H1 c2c diagnostic showed cross-shard fetch_or vs. owner
        // swap on adjacent atomics bounced cache lines between cores.
        let inbound_dirty: Vec<Arc<crate::shard::CachePadded<AtomicU64>>> = (0..n)
            .map(|_| Arc::new(crate::shard::CachePadded::new(AtomicU64::new(0))))
            .collect();

        // Shared pub/sub channel registry (one per server, read on every PUBLISH).
        let pubsub: PubSubReg = Arc::new(RwLock::new(HashMap::new()));
        // Shared pub/sub pattern registry. Empty in steady state — the
        // channel-only PUBLISH path skips the walk when so.
        let pubsub_patterns: PubSubPatternReg = Arc::new(RwLock::new(Vec::new()));

        // Reconcile the on-disk shard layout (count + routing) before any
        // shard loads its files; a mismatch re-homes every key once, here.
        // Skipped for a pure in-memory run against a dir with no kevy files.
        // Cluster mode always records the layout even with AOF off and an
        // empty dir: a later SAVE writes slot-distributed `dump-{i}.rdb`, and
        // without a meta a non-cluster restart would read them as KevyHash
        // and silently strand every key.
        if self.enable_aof
            || self.cluster_port_base.is_some()
            || crate::reshard::has_kevy_files(&self.data_dir)
        {
            let routing = if self.cluster_port_base.is_some() {
                kevy_persist::Routing::Slots
            } else {
                kevy_persist::Routing::KevyHash
            };
            crate::reshard::ensure_layout(&self.data_dir, n, routing, &self.commands)?;
        }

        // Advertised cluster topology (None = cluster off). A 0.0.0.0 bind
        // advertises 127.0.0.1 — an unroutable redirect target would strand
        // every cluster client (single-machine scope; no announce-ip knob).
        let topo = self.cluster_port_base.map(|base| crate::cluster::ClusterTopo {
            ip: if self.ip == [0, 0, 0, 0] { [127, 0, 0, 1] } else { self.ip },
            port_base: base,
        });

        // Build every shard up front so a bind/open failure aborts before we spawn.
        let mut shards = Vec::with_capacity(n);
        for id in 0..n {
            let listener = tcp_listen_reuseport(self.ip, self.port, 1024)?;
            // Cluster mode: a second, deterministic per-shard listener at
            // port_base + id (plain bind — exactly one owner per port).
            let cluster_listener = match self.cluster_port_base {
                Some(base) => Some(kevy_sys::tcp_listen(self.ip, base + id as u16, 1024)?),
                None => None,
            };
            // Replication listener (per Issue Ledger I2): per-shard
            // deterministic port, same `tcp_listen` (no SO_REUSEPORT)
            // pattern as cluster. A replica's shard-aware client will
            // connect to every `base + id` to mirror the full keyspace.
            let replication_listener = match self.replication_port_base {
                Some(base) => Some(kevy_sys::tcp_listen(self.ip, base + id as u16, 1024)?),
                None => None,
            };
            let aof = if self.enable_aof {
                Some(Aof::open(
                    &kevy_persist::layout::aof_path(&self.data_dir, id),
                    self.appendfsync,
                )?)
            } else {
                None
            };
            let mut store = Store::new();
            // The reactor loop refreshes the store clock once per batch, so
            // lazy expiry can trust the cached clock (skip per-command
            // `Instant::now()`).
            store.set_cached_clock(true);
            // v1.25 A.3: hand the bio-drop channel sender to the store so
            // SET overwrites of heavy values (Arc<[u8]> ≥ 256 B, non-empty
            // collections) get freed off-reactor. Sender clone is cheap
            // (`Arc::clone`); the bio thread is shared across all shards
            // (B2 single-global, mirrors valkey `bio.c`).
            store.set_bio_drop_sender(bio_send.clone());
            self.commands.on_shard_init(&mut store);
            shards.push(Shard {
                id,
                nshards: n,
                cluster: topo.clone(),
                cluster_listener,
                store,
                commands: self.commands.clone(),
                poller: Poller::new()?,
                listener,
                waker: wakers[id].clone(),
                inboxes: std::mem::take(&mut inboxes[id]),
                outboxes: std::mem::take(&mut outboxes[id]),
                backlog: (0..n).map(|_| VecDeque::new()).collect(),
                wakers: wakers.clone(),
                conns: KevyMap::new(),
                arm_pending: Vec::new(),
                fd_to_conn: KevyMap::new(),
                next_conn_id: 1,
                events: Vec::with_capacity(1024),
                read_buf: vec![0u8; 64 * 1024],
                pending_wakes: 0,
                backlog_nonempty: 0,
                request_batch_nonempty: 0,
                publish_batch_nonempty: 0,
                parked: parked.clone(),
                inbound_dirty: inbound_dirty.clone(),
                data_dir: self.data_dir.clone(),
                aof,
                replicate: if self.enable_replication {
                    Some(kevy_replicate::source::ReplicationSource::new(
                        usize::try_from(self.replication_buffer_size)
                            .unwrap_or(usize::MAX),
                    ))
                } else {
                    None
                },
                replication_listener,
                replicas: Vec::new(),
                slots: kevy_replicate::slot::SlotTable::new(),
                replication_reconnect_window_ms: self.replication_reconnect_window_ms,
                replication_epoch: std::time::Instant::now(),
                replica_inbox: self.replica_inboxes.get_mut(id).and_then(Option::take),
                replica_snapshot_buf: Vec::new(),
                persist: crate::persist_worker::PersistWorker::new(),
                auto_aof_rewrite_pct: self.auto_aof_rewrite_pct,
                auto_aof_rewrite_min_size: self.auto_aof_rewrite_min_size,
                dirty: Vec::new(),
                pubsub: pubsub.clone(),
                pubsub_patterns: pubsub_patterns.clone(),
                psub_local: HashMap::new(),
                subs_by_channel: HashMap::new(),
                publish_batch: (0..n).map(|_| Vec::new()).collect(),
                request_batch: (0..n).map(|_| Vec::new()).collect(),
                // Seed from the live config at construction, not default():
                // these flags were otherwise blind until the first 100 ms
                // shard tick, so a write landing before that never fired
                // its keyspace notification (CI-visible flake; a real
                // startup gap for any pre-configured notify_keyspace_events).
                notify_flags: self
                    .commands
                    .live_runtime_config()
                    .notify_flags
                    .unwrap_or_default(),
                spin_limit: self.spin_limit,
                // `Poller::wait` takes the timeout as `i32` (POSIX
                // poll/epoll convention). The config knob is `u32` —
                // we clamp to i32::MAX, far above any sane park-timeout.
                park_timeout_ms: self.park_timeout_ms.min(i32::MAX as u32) as i32,
                tick_check_every: self.tick_check_every,
                slowlog: crate::exec_slowlog::SlowlogState::new(
                    self.slowlog_slower_than_micros,
                    self.slowlog_max_len,
                ),
                blocked: crate::blocked::BlockedClients::new(),
                origin_blocks: std::collections::HashMap::new(),
                xwaiters: crate::block_xshard::XShardWaiters::default(),
                reply_scratch: Vec::with_capacity(4096),
                argv_pool: kevy_resp::ArgvPool::new(),
            });
        }

        // Reactor selection on Linux:
        //   KEVY_IO_URING unset → auto: try io_uring, fall back to epoll if the
        //     host can't build the ring (probe below) — startup never fails.
        //   KEVY_IO_URING=0/off/no/false → force the epoll readiness reactor.
        //   KEVY_IO_URING=<anything else> → force io_uring (no fallback; a
        //     setup failure then surfaces loudly — for benchmarks / tests).
        // The probe creates+drops a real ring with the run_uring parameters, so
        // it catches a seccomp-blocked io_uring_setup (Docker's default profile)
        // and pre-5.19 kernels before any shard loads data. (macOS = kqueue.)
        #[cfg(target_os = "linux")]
        let use_uring = match std::env::var("KEVY_IO_URING").ok().as_deref() {
            Some("0") | Some("off") | Some("no") | Some("false") => false,
            Some(_) => true,
            None => {
                let avail = crate::uring_reactor::io_uring_available();
                eprintln!(
                    "kevy: reactor = {} (io_uring {})",
                    if avail { "io_uring" } else { "epoll" },
                    if avail { "available" } else { "unavailable — kernel <5.19 or seccomp; using epoll" },
                );
                avail
            }
        };

        // v1.18.0: the replication listener + accept path is wired only
        // through the epoll/kqueue reactor (`shard.run`); the io_uring
        // T1.12.5: io_uring + replication is now supported. The
        // replication-adjacent work (accept / read / write / pump /
        // slot+view+watermark ticks) is poll-driven from the io_uring
        // reactor's tick path (mostly per-tick @ 10 Hz, with
        // `pump_replication` + `reap_closed_replicas` per-iter via
        // their own early returns when nothing's live). Throughput
        // path stays io_uring-native — only replica metadata uses
        // polling. See `Shard::run_uring`.

        let mut handles = Vec::with_capacity(n);
        for shard in shards {
            let stop = stop.clone();
            let id = shard.id;
            handles.push(std::thread::spawn(move || {
                #[cfg(target_os = "linux")]
                let res = if use_uring { shard.run_uring(stop) } else { shard.run(stop) };
                #[cfg(not(target_os = "linux"))]
                let res = shard.run(stop);
                if let Err(e) = res {
                    eprintln!("kevy: shard {id} exited with error: {e}");
                }
            }));
        }
        for h in handles {
            let _ = h.join();
        }
        // v1.25 A.3 shutdown: every shard has joined → every cloned
        // sender on every Store has been dropped. Drop the last live
        // sender (this fn's `bio_send`) so the channel closes; the bio
        // thread's `recv()` returns Err and it exits its loop. The
        // `join()` then blocks until that exit completes — guarding
        // against process tear-down while a final large free is in
        // flight (an unsafe wrt `madvise`/`munmap` semantics — the
        // kernel needs the process alive to actually release pages).
        drop(bio_send);
        let _ = bio_handle.join();
        Ok(())
    }
}
