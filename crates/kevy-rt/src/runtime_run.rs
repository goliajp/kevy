//! [`Runtime::run`] — validate config, build the cross-shard mesh and
//! every [`Shard`], spawn one thread per core, and join. Cold startup
//! path (runs once per process); split out of `runtime.rs` for the
//! 500-LOC house rule and decomposed into per-stage helpers.

use crate::Commands;
use crate::message::{Inbound, PubSubPatternReg, PubSubReg};
use crate::runtime::Runtime;
use crate::shard::{CachePadded, Shard};
use kevy_map::KevyMap;
use kevy_persist::Aof;
use kevy_ring::{Consumer, Producer};
use kevy_store::Store;
use kevy_sys::{Poller, Waker, tcp_listen_reuseport, waker};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, RwLock};

/// Cross-shard shared state built once before any shard spawns: the
/// per-core-pair SPSC ring mesh, wakers, park flags, inbox-dirty
/// bitmaps, and the pub/sub registries.
struct Shared {
    /// `outboxes[i][j]` = shard i's producer half toward shard j.
    outboxes: Vec<Vec<Option<Producer<Inbound>>>>,
    /// `inboxes[j][i]` = shard j's consumer half from shard i.
    inboxes: Vec<Vec<Option<Consumer<Inbound>>>>,
    wakers: Vec<Arc<Waker>>,
    parked: Vec<Arc<CachePadded<AtomicBool>>>,
    inbound_dirty: Vec<Arc<CachePadded<AtomicU64>>>,
    /// Shared pub/sub channel registry (one per server, read on every
    /// PUBLISH) + the pattern registry (empty in steady state — the
    /// channel-only PUBLISH path skips the walk when so).
    pubsub: PubSubReg,
    pubsub_patterns: PubSubPatternReg,
}

impl Shared {
    fn build(n: usize, ring_capacity: usize) -> io::Result<Shared> {
        // One lock-free SPSC ring per ordered core-pair (i→j): the producer
        // goes to shard i's outbox[j], the consumer to shard j's inbox[i].
        // There is no self-ring — a shard runs its own commands inline,
        // never over a ring.
        let mut outboxes: Vec<Vec<Option<Producer<Inbound>>>> =
            (0..n).map(|_| (0..n).map(|_| None).collect()).collect();
        let mut inboxes: Vec<Vec<Option<Consumer<Inbound>>>> =
            (0..n).map(|_| (0..n).map(|_| None).collect()).collect();
        for i in 0..n {
            for j in 0..n {
                if i == j {
                    continue;
                }
                let (p, c) = kevy_ring::ring::<Inbound>(ring_capacity);
                outboxes[i][j] = Some(p);
                inboxes[j][i] = Some(c);
            }
        }
        let mut wakers: Vec<Arc<Waker>> = Vec::with_capacity(n);
        for _ in 0..n {
            wakers.push(Arc::new(waker()?));
        }
        let parked: Vec<Arc<CachePadded<AtomicBool>>> = (0..n)
            .map(|_| Arc::new(CachePadded::new(AtomicBool::new(false))))
            .collect();
        // Per-shard inbox-dirty bitmaps (one u64 bit per peer src).
        // Senders OR a bit on the target's dirty word; the target's
        // `drain_inbound_core` swaps and short-circuits when 0.
        assert!(
            n <= 64,
            "kevy-rt: shard count {n} exceeds 64 — inbound_dirty bitmap holds one bit per peer in a u64. Reduce --threads or extend to a multi-word bitmap.",
        );
        // Pad each Arc<AtomicU64> to a full 64-byte cache
        // line. A `perf c2c` diagnostic showed cross-shard fetch_or vs. owner
        // swap on adjacent atomics bounced cache lines between cores.
        let inbound_dirty: Vec<Arc<CachePadded<AtomicU64>>> = (0..n)
            .map(|_| Arc::new(CachePadded::new(AtomicU64::new(0))))
            .collect();
        Ok(Shared {
            outboxes,
            inboxes,
            wakers,
            parked,
            inbound_dirty,
            pubsub: Arc::new(RwLock::new(HashMap::new())),
            pubsub_patterns: Arc::new(RwLock::new(Vec::new())),
        })
    }
}

impl<C: Commands> Runtime<C> {
    /// Spawn one thread per shard and run until `stop` is set.
    pub fn run(mut self, stop: Arc<AtomicBool>) -> io::Result<()> {
        let n = self.nshards;
        // Single global bio thread. Spawn BEFORE shards so
        // every shard's first overwrite already has a live consumer. The
        // held `bio_send` is cloned into every Store below; shutdown
        // ordering (shards join → Stores drop their Senders → this fn's
        // `bio_send` drops → channel closes → bio thread exits → its
        // `join()` below completes) guards against process tear-down
        // while a final large free is in flight (`madvise`/`munmap`
        // need the process alive). See `crate::bio` for the rationale.
        let (bio_send, bio_handle) = crate::bio::spawn();
        self.validate_port_ranges(n)?;
        let mut shared = Shared::build(n, self.ring_capacity)?;
        self.reconcile_layout(n)?;
        // UDS listener: only ONE per server (no SO_REUSEPORT for AF_UNIX),
        // so it lives on shard 0. Bound up-front so a bind failure aborts
        // before any shard spawns.
        let mut unix_listener: Option<kevy_sys::Socket> = None;
        if let Some(p) = self.unix_socket_path.as_ref() {
            let path_bytes = p.to_string_lossy();
            unix_listener = Some(kevy_sys::unix_listen(path_bytes.as_bytes(), 1024)?);
        }
        // Build every shard up front so a bind/open failure aborts before
        // we spawn.
        let shards = self.build_shards(n, &mut shared, &bio_send, unix_listener)?;
        let (use_uring, uring_forced) = reactor_choice();
        let mut handles = Vec::with_capacity(n);
        for shard in shards {
            let stop = stop.clone();
            handles.push(std::thread::spawn(move || {
                run_shard_thread(shard, stop, use_uring, uring_forced);
            }));
        }
        for h in handles {
            let _ = h.join();
        }
        // Bio shutdown: see the bio-spawn comment above.
        drop(bio_send);
        let _ = bio_handle.join();
        Ok(())
    }

    /// Reject a cluster / replication port range that overflows u16 up
    /// front (loud) instead of wrapping a listener onto a low/privileged
    /// port while CLUSTER SLOTS advertises 65536+.
    fn validate_port_ranges(&self, n: usize) -> io::Result<()> {
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
        Ok(())
    }

    /// Reconcile the on-disk shard layout (count + routing) before any
    /// shard loads its files; a mismatch re-homes every key once, here.
    /// Skipped for a pure in-memory run against a dir with no kevy files.
    /// Cluster mode always records the layout even with AOF off and an
    /// empty dir: a later SAVE writes slot-distributed `dump-{i}.rdb`, and
    /// without a meta a non-cluster restart would read them as KevyHash
    /// and silently strand every key.
    fn reconcile_layout(&self, n: usize) -> io::Result<()> {
        if self.enable_aof
            || self.cluster_port_base.is_some()
            || crate::reshard::has_kevy_files(&self.data_dir)
        {
            let routing = if self.cluster_port_base.is_some() {
                kevy_persist::Routing::Slots
            } else {
                kevy_persist::Routing::KevyHash
            };
            crate::reshard::ensure_layout(
                &self.data_dir,
                n,
                routing,
                &self.commands,
                self.resolved_tier_budget(),
                &self.tier_root(),
            )?;
        }
        Ok(())
    }

    /// Advertised cluster topology (None = cluster off). A 0.0.0.0 bind
    /// advertises 127.0.0.1 — an unroutable redirect target would strand
    /// every cluster client (single-machine scope; no announce-ip knob).
    fn cluster_topo(&self) -> Option<crate::cluster::ClusterTopo> {
        self.cluster_port_base
            .map(|base| crate::cluster::ClusterTopo {
                ip: if self.ip == [0, 0, 0, 0] {
                    [127, 0, 0, 1]
                } else {
                    self.ip
                },
                port_base: base,
            })
    }

    /// Build all `n` shards: per-shard listeners + store + the flat
    /// `Shard` field-init. Field-by-field comments live with the struct
    /// definition in [`crate::shard`].
    // LOC-WAIVER: flat per-shard construction table — listener/socket
    // setup then one line per Shard field; no control flow to split.
    fn build_shards(
        &mut self,
        n: usize,
        shared: &mut Shared,
        bio_send: &kevy_store::BioDropSender,
        mut unix_listener: Option<kevy_sys::Socket>,
    ) -> io::Result<Vec<Shard<C>>> {
        let topo = self.cluster_topo();
        let mut shards = Vec::with_capacity(n);
        for id in 0..n {
            let arms_accept = self.accept_shards.is_none_or(|k| id < k);
            // Off-accept-set shards skip the SO_REUSEPORT bind so
            // the kernel routes new conns only to the armed subset.
            let listener = if arms_accept {
                Some(tcp_listen_reuseport(self.ip, self.port, 1024)?)
            } else {
                None
            };
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
                Some(Aof::open_with_repair(
                    &kevy_persist::layout::aof_path(&self.data_dir, id),
                    self.appendfsync,
                    self.replay_resync,
                )?)
            } else {
                None
            };
            let mut store = Store::new();
            // The reactor loop refreshes the store clock once per batch, so
            // lazy expiry can trust the cached clock (skip per-command
            // `Instant::now()`).
            store.set_cached_clock(true);
            // Hand the bio-drop channel sender to the store so
            // SET overwrites of heavy values (Arc<[u8]> ≥ 256 B, non-empty
            // collections) get freed off-reactor. Sender clone is cheap
            // (`Arc::clone`); the bio thread is shared across all shards
            // (single global thread, mirrors valkey `bio.c`).
            store.set_bio_drop_sender(bio_send.clone());
            // Tiering: the process budget — resolved
            // bytes from the builder (`[tiering]` TOML/CLI/env full
            // surface), or the minimal `KEVY_TIER_BUDGET` plain-bytes
            // env knob — split evenly across shards; per-shard cold
            // tier under `<tier root>/<id>`.
            if let Some(total) = self.resolved_tier_budget() {
                store.enable_tiering(
                    &self.tier_root().join(id.to_string()),
                    Self::per_shard_tier_budget(total, n),
                )?;
            }
            self.commands.on_shard_init(&mut store);
            shards.push(Shard {
                #[cfg(target_os = "linux")]
                aof_offload: Default::default(),
                #[cfg(target_os = "linux")]
                held_responses: Vec::new(),
                rewrite_handoff: None,
                rewrite_rate_mark: None,
                rewrite_calm_ticks: 0,
                xshard_inflight: 0,
                id,
                nshards: n,
                cluster: topo.clone(),
                cluster_listener,
                // UDS: only shard 0 holds the (single) unix listener.
                unix_listener: if id == 0 { unix_listener.take() } else { None },
                store,
                commands: self.commands.clone(),
                poller: Poller::new()?,
                listener,
                waker: shared.wakers[id].clone(),
                inboxes: std::mem::take(&mut shared.inboxes[id]),
                outboxes: std::mem::take(&mut shared.outboxes[id]),
                backlog: (0..n).map(|_| VecDeque::new()).collect(),
                wakers: shared.wakers.clone(),
                conns: KevyMap::new(),
                arm_pending: Vec::new(),
                closing_uring_conns: Vec::new(),
                fd_to_conn: KevyMap::new(),
                // Conn ids stride by shard count from a per-shard
                // start, so every id is unique across the whole
                // instance (CLIENT ID / CLIENT KILL ID contract) and
                // still allocation-free per accept.
                next_conn_id: id as u64 + 1,
                conn_id_step: n as u64,
                events: Vec::with_capacity(1024),
                read_buf: vec![0u8; 64 * 1024],
                pending_wakes: 0,
                backlog_nonempty: 0,
                request_batch_nonempty: 0,
                publish_batch_nonempty: 0,
                parked: shared.parked.clone(),
                inbound_dirty: shared.inbound_dirty.clone(),
                data_dir: self.data_dir.clone(),
                aof,
                replicate: if self.enable_replication || self.feed_enabled {
                    let budget = if self.feed_enabled {
                        self.replication_buffer_size.max(self.feed_buffer_size)
                    } else {
                        self.replication_buffer_size
                    };
                    let boot = kevy_persist::feed_meta::load_feed_boot(&self.data_dir, id)?;
                    let mut src = kevy_replicate::source::ReplicationSource::new(
                        usize::try_from(budget).unwrap_or(usize::MAX),
                    );
                    src.set_next_offset(boot.next_offset);
                    Some(kevy_replicate::feed::FeedSource::new(boot.generation, src))
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
                replica_applied_next: 0,
                repl_waiters: Vec::new(),
                seen_promotion_epoch: None,
                persist: crate::persist_worker::PersistWorker::new(),
                auto_aof_rewrite_pct: self.auto_aof_rewrite_pct,
                auto_aof_rewrite_bytes: self.auto_aof_rewrite_bytes,
                auto_aof_rewrite_interval_secs: self.auto_aof_rewrite_interval_secs,
                replay_resync: self.replay_resync,
                auto_aof_rewrite_min_size: self.auto_aof_rewrite_min_size,
                dirty: Vec::new(),
                pubsub: shared.pubsub.clone(),
                pubsub_patterns: shared.pubsub_patterns.clone(),
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
                arms_accept: self.accept_shards.is_none_or(|n| id < n),
                max_clients_per_shard: if self.max_clients == 0 {
                    0
                } else {
                    self.max_clients.div_ceil(n)
                },
                rejected_connections: 0,
                input_hard_limit: std::env::var("KEVY_DEBUG_INPUT_LIMIT")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(crate::CLIENT_INPUT_HARD_LIMIT),
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
                serve_confirm: std::collections::HashMap::new(),
                reply_scratch: Vec::with_capacity(4096),
                argv_pool: kevy_resp::ArgvPool::new(),
            });
        }
        Ok(shards)
    }
}

/// Reactor selection on Linux:
///   KEVY_IO_URING unset → auto: try io_uring, fall back to epoll if the
///     host can't build the ring (probe below) — startup never fails.
///   KEVY_IO_URING=0/off/no/false → force the epoll readiness reactor.
///   KEVY_IO_URING=<anything else> → force io_uring (no fallback; a
///     setup failure then surfaces loudly — for benchmarks / tests).
/// The probe creates+drops a real ring with the run_uring parameters, so
/// it catches a seccomp-blocked io_uring_setup (Docker's default profile)
/// and pre-5.19 kernels before any shard loads data. (macOS = kqueue.)
#[cfg(target_os = "linux")]
fn reactor_choice() -> (bool, bool) {
    match std::env::var("KEVY_IO_URING").ok().as_deref() {
        Some("0") | Some("off") | Some("no") | Some("false") => (false, true),
        Some(_) => (true, true),
        None => {
            let avail = crate::uring_reactor::io_uring_available();
            eprintln!(
                "kevy: reactor = {} (io_uring {})",
                if avail { "io_uring" } else { "epoll" },
                if avail {
                    "available"
                } else {
                    "unavailable — kernel <5.19 or seccomp; using epoll"
                },
            );
            (avail, false)
        }
    }
}

/// Non-Linux: always the readiness reactor (kqueue on macOS).
#[cfg(not(target_os = "linux"))]
fn reactor_choice() -> (bool, bool) {
    (false, false)
}

/// One shard thread's body: pick the reactor and run it to completion.
///
/// Per-shard ring setup is attempted BEFORE committing to the
/// io_uring path. The global probe proves one ring builds; N shards
/// need N rings, and a late failure (ENOMEM under pressure) used to
/// kill the shard thread and leave a half-dead server (found via
/// GH-runner CI: blocking_cross_shard hangs). Auto mode now degrades
/// that shard to epoll, loudly. A forced KEVY_IO_URING=1 keeps the
/// old fail-loud contract.
fn run_shard_thread<C: Commands>(
    shard: Shard<C>,
    stop: Arc<AtomicBool>,
    use_uring: bool,
    uring_forced: bool,
) {
    let id = shard.id;
    #[cfg(target_os = "linux")]
    let res = if use_uring {
        match crate::uring_reactor::build_uring() {
            Ok(pair) => shard.run_uring(pair, stop),
            Err(e) if !uring_forced => {
                eprintln!(
                    "kevy: shard {id}: io_uring setup failed ({e}); \
                     falling back to the epoll reactor for this shard"
                );
                shard.run(stop)
            }
            Err(e) => Err(e),
        }
    } else {
        shard.run(stop)
    };
    #[cfg(not(target_os = "linux"))]
    let res = {
        let _ = (use_uring, uring_forced);
        shard.run(stop)
    };
    if let Err(e) = res {
        eprintln!("kevy: shard {id} exited with error: {e}");
    }
}
