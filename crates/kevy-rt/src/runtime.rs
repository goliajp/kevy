//! The public entry point: configure and run the thread-per-core server.

use crate::Commands;
use crate::message::{Inbound, PubSubReg};
use crate::shard::Shard;
use kevy_hash::FxHashMap;
use kevy_persist::{Aof, Fsync};
use kevy_ring::{Consumer, Producer};
use kevy_store::Store;
use kevy_sys::{Poller, Waker, tcp_listen_reuseport, waker};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};

/// Slots in each per-core-pair SPSC ring. A full ring spills to a local backlog
/// (see [`Shard`]), so this only bounds the lock-free fast path, not capacity.
const RING_CAPACITY: usize = 1024;

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
        }
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
                let (p, c) = kevy_ring::ring::<Inbound>(RING_CAPACITY);
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

        // Build every shard up front so a bind/open failure aborts before we spawn.
        let mut shards = Vec::with_capacity(n);
        for id in 0..n {
            let listener = tcp_listen_reuseport(self.ip, self.port, 1024)?;
            let aof = if self.enable_aof {
                Some(Aof::open(
                    &self.data_dir.join(format!("aof-{id}.aof")),
                    Fsync::EverySec,
                )?)
            } else {
                None
            };
            shards.push(Shard {
                id,
                nshards: n,
                store: Store::new(),
                commands: self.commands.clone(),
                poller: Poller::new()?,
                listener,
                waker: wakers[id].clone(),
                inboxes: std::mem::take(&mut inboxes[id]),
                outboxes: std::mem::take(&mut outboxes[id]),
                backlog: (0..n).map(|_| VecDeque::new()).collect(),
                wakers: wakers.clone(),
                conns: FxHashMap::default(),
                fd_to_conn: FxHashMap::default(),
                next_conn_id: 1,
                events: Vec::with_capacity(1024),
                read_buf: vec![0u8; 64 * 1024],
                pending_wakes: vec![false; n],
                parked: parked.clone(),
                data_dir: self.data_dir.clone(),
                aof,
                dirty: Vec::new(),
                pubsub: pubsub.clone(),
                publish_batch: (0..n).map(|_| Vec::new()).collect(),
                request_batch: (0..n).map(|_| Vec::new()).collect(),
            });
        }

        // Opt into the Linux io_uring (completion) reactor with KEVY_IO_URING=1;
        // otherwise use the readiness reactor (epoll/kqueue), the default + macOS.
        #[cfg(target_os = "linux")]
        let use_uring = std::env::var_os("KEVY_IO_URING").is_some();

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
