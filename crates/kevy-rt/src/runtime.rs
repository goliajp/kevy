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
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};

/// Default slots in each per-core-pair SPSC ring. A full ring spills
/// to a local backlog (see [`Shard`]), so this only bounds the
/// lock-free fast path, not capacity. Overridable via the
/// `[advanced] ring_capacity` config field threaded through
/// [`Runtime::with_advanced`].
const DEFAULT_RING_CAPACITY: usize = 1024;

/// The public entry point: configure and run the thread-per-core server.
pub struct Runtime<C: Commands> {
    ip: [u8; 4],
    port: u16,
    nshards: usize,
    commands: C,
    /// Directory for per-shard snapshot files (`dump-<id>.rdb`) and AOF logs.
    data_dir: PathBuf,
    /// Whether the append-only log is enabled.
    enable_aof: bool,
    /// fsync policy for the AOF. Default `EverySec` matches Redis.
    appendfsync: Fsync,
    /// auto-trigger BGREWRITEAOF when AOF grew this many % above the size
    /// at the previous rewrite. `0` disables. Default `100` (matches Redis).
    auto_aof_rewrite_pct: u32,
    /// Floor below which auto-rewrite is skipped. Default `64 MiB`.
    auto_aof_rewrite_min_size: u64,
    /// Reactor SPSC ring slot count. See [`DEFAULT_RING_CAPACITY`].
    ring_capacity: usize,
    /// Reactor busy-poll iter limit before parking. Stored as `u32`
    /// for the per-shard counter; the [`Shard`] field carries it
    /// forward into the loop.
    spin_limit: u32,
    /// Reactor blocking-wait timeout in ms when parked.
    park_timeout_ms: u32,
    /// Wall-clock-read throttle for the tick check (TTL reaper / live
    /// config refresh / auto-AOF-rewrite).
    tick_check_every: u32,
    /// `[slowlog].slower_than_micros`. Default: `-1` (OFF — zero
    /// hot-path cost: every command would otherwise pay an
    /// `Instant::now()` pair around dispatch). Set to `10_000` to match
    /// Redis's default 10 ms threshold; see [`Self::with_slowlog`] /
    /// `CONFIG SET slowlog-log-slower-than 10000`.
    slowlog_slower_than_micros: i64,
    /// `[slowlog].max_len`. Per-shard cap.
    slowlog_max_len: u32,
}

impl<C: Commands> Runtime<C> {
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
        }
    }

    /// SLOWLOG tuning (`[slowlog]` config section). Default
    /// `slower_than_micros = -1` (OFF) so the hot path never reads the
    /// clock — every enabled command otherwise pays an `Instant::now()`
    /// pair around dispatch, ~30 ns/op (≈9 % at 3 M ops/s). To match
    /// Redis's 10 ms default, pass `10_000`; `0` records all; `-1`
    /// disables. `max_len` is the per-shard ring cap (default 128).
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
    pub fn with_data_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.data_dir = dir.into();
        self
    }

    /// Enable/disable the append-only log. Default: enabled.
    pub fn with_aof(mut self, on: bool) -> Self {
        self.enable_aof = on;
        self
    }

    /// fsync policy for the AOF. Default `EverySec` matches Redis (lose at
    /// most ~1 s of writes on a crash). `Always` is zero-loss but ~50 %
    /// throughput; `No` defers everything to the OS pagecache.
    pub fn with_appendfsync(mut self, fsync: Fsync) -> Self {
        self.appendfsync = fsync;
        self
    }

    /// Auto-trigger BGREWRITEAOF when the live AOF has grown by at least
    /// `pct` percent above its size at the previous rewrite, AND is at
    /// least `min_size` bytes. `pct=0` disables auto-rewrite (clients can
    /// still run BGREWRITEAOF manually). Defaults: 100 % / 64 MiB.
    pub fn with_auto_aof_rewrite(mut self, pct: u32, min_size: u64) -> Self {
        self.auto_aof_rewrite_pct = pct;
        self.auto_aof_rewrite_min_size = min_size;
        self
    }

    /// Spawn one thread per shard and run until `stop` is set.
    pub fn run(self, stop: Arc<AtomicBool>) -> io::Result<()> {
        let n = self.nshards;

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
        let parked: Vec<Arc<AtomicBool>> =
            (0..n).map(|_| Arc::new(AtomicBool::new(false))).collect();

        // Shared pub/sub channel registry (one per server, read on every PUBLISH).
        let pubsub: PubSubReg = Arc::new(RwLock::new(HashMap::new()));
        // Shared pub/sub pattern registry. Empty in steady state — the
        // channel-only PUBLISH path skips the walk when so.
        let pubsub_patterns: PubSubPatternReg = Arc::new(RwLock::new(Vec::new()));

        // Build every shard up front so a bind/open failure aborts before we spawn.
        let mut shards = Vec::with_capacity(n);
        for id in 0..n {
            let listener = tcp_listen_reuseport(self.ip, self.port, 1024)?;
            let aof = if self.enable_aof {
                Some(Aof::open(
                    &self.data_dir.join(format!("aof-{id}.aof")),
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
            self.commands.on_shard_init(&mut store);
            shards.push(Shard {
                id,
                nshards: n,
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
                fd_to_conn: KevyMap::new(),
                next_conn_id: 1,
                events: Vec::with_capacity(1024),
                read_buf: vec![0u8; 64 * 1024],
                pending_wakes: vec![false; n],
                parked: parked.clone(),
                data_dir: self.data_dir.clone(),
                aof,
                auto_aof_rewrite_pct: self.auto_aof_rewrite_pct,
                auto_aof_rewrite_min_size: self.auto_aof_rewrite_min_size,
                dirty: Vec::new(),
                pubsub: pubsub.clone(),
                pubsub_patterns: pubsub_patterns.clone(),
                psub_local: HashMap::new(),
                publish_batch: (0..n).map(|_| Vec::new()).collect(),
                request_batch: (0..n).map(|_| Vec::new()).collect(),
                notify_flags: crate::NotificationFlags::default(),
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
        Ok(())
    }
}
