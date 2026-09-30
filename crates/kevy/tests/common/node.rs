//! An in-process server for tests that restart it on the same directory
//! or follow it with a replica: a primary with the AOF on and a
//! replication listener, or a replica of one.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use kevy_tmpdir::TmpDir;

use super::Wire;

pub struct Node {
    pub port: u16,
    pub replication_base: u16,
    /// Shard `i`'s own listener, when the node has them.
    pub shard_ports: Vec<u16>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Node {
    /// A primary on `dir` with the AOF on and a replication listener.
    pub fn primary(nshards: usize, dir: &TmpDir) -> Self {
        Self::primary_with(nshards, dir, false)
    }

    /// [`Self::primary`] with a listener per shard (cluster mode), so a
    /// test places a connection on the shard it wants on every platform.
    #[allow(dead_code)] // not every test binary places connections
    pub fn primary_on_shard_ports(nshards: usize, dir: &TmpDir) -> Self {
        Self::primary_with(nshards, dir, true)
    }

    /// [`Self::primary`] again on the ports of a primary that stopped, so
    /// its replicas reconnect to it.
    #[allow(dead_code)] // not every test binary restarts a primary
    pub fn primary_restarted(port: u16, nshards: usize, dir: &TmpDir) -> Self {
        Self::primary_at(port, nshards, dir, false)
    }

    fn primary_with(nshards: usize, dir: &TmpDir, shard_ports: bool) -> Self {
        let base = kevy_testnet::free_port_block(2 * nshards);
        Self::primary_at(base, nshards, dir, shard_ports)
    }

    fn primary_at(base: u16, nshards: usize, dir: &TmpDir, shard_ports: bool) -> Self {
        let (port, replication_base) = (base, base + 1);
        let cluster_base = base + 1 + nshards as u16;
        let shard_ports: Vec<u16> = if shard_ports {
            (0..nshards as u16).map(|i| cluster_base + i).collect()
        } else {
            Vec::new()
        };
        let cluster = (!shard_ports.is_empty()).then_some(cluster_base);
        // the replication listener runs on the epoll / kqueue reactor
        // SAFETY: set before any runtime thread of this test reads it
        unsafe {
            std::env::set_var("KEVY_IO_URING", "0");
        }
        let dir = dir.path().to_path_buf();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let handle = std::thread::spawn(move || {
            let rt = kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(nshards))
                .bind([127, 0, 0, 1], port)
                .shards(nshards)
                .with_data_dir(dir)
                .with_replication(true)
                .with_replication_buffer_size(1024 * 1024)
                .with_replication_listener(replication_base);
            let rt = match cluster {
                Some(b) => rt.with_cluster(b),
                None => rt,
            };
            rt.run(stop_thread).unwrap();
        });
        let replication = (0..nshards as u16).map(|i| replication_base + i);
        for p in std::iter::once(port).chain(replication).chain(shard_ports.iter().copied()) {
            kevy_testnet::assert_listening(p, "the primary");
        }
        Self { port, replication_base, shard_ports, stop, handle: Some(handle) }
    }

    /// A replica with `primary`'s shard count, following it.
    pub fn replica(primary: &Node, nshards: usize, dir: &TmpDir) -> Self {
        let commands = kevy::KevyCommands::sharded(nshards);
        let inboxes = commands.state().take_replica_inboxes().expect("fresh state");
        let port = kevy_testnet::free_port();
        let dir = dir.path().to_path_buf();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let handle = std::thread::spawn(move || {
            let rt = kevy_rt::Runtime::builder(commands)
                .bind([127, 0, 0, 1], port)
                .shards(nshards)
                .with_data_dir(dir)
                .with_aof(false)
                .with_replica_inboxes(inboxes);
            rt.run(stop_thread).unwrap();
        });
        kevy_testnet::assert_listening(port, "the replica");
        let node =
            Self { port, replication_base: 0, shard_ports: Vec::new(), stop, handle: Some(handle) };
        let upstream = primary.replication_base.to_string();
        let reply = node.wire().call(&[b"REPLICAOF" as &[u8], b"127.0.0.1", upstream.as_bytes()]);
        assert_eq!(reply, b"+OK\r\n");
        node
    }

    pub fn wire(&self) -> Wire {
        self.wire_on(self.port)
    }

    /// A connection to `port`, one of this node's listeners.
    pub fn wire_on(&self, port: u16) -> Wire {
        let s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(20))).unwrap();
        Wire::new(s)
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = std::net::TcpStream::connect(("127.0.0.1", self.port));
        if let Some(h) = self.handle.take() {
            h.join().unwrap();
        }
    }
}
