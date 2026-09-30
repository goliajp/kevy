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
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Node {
    /// A primary on `dir` with the AOF on and a replication listener.
    pub fn primary(nshards: usize, dir: &TmpDir) -> Self {
        let base = kevy_testnet::free_port_block(nshards);
        let (port, replication_base) = (base, base + 1);
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
            rt.run(stop_thread).unwrap();
        });
        for p in std::iter::once(port).chain((0..nshards as u16).map(|i| replication_base + i)) {
            kevy_testnet::assert_listening(p, "the primary");
        }
        Self { port, replication_base, stop, handle: Some(handle) }
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
        let node = Self { port, replication_base: 0, stop, handle: Some(handle) };
        let upstream = primary.replication_base.to_string();
        let reply = node.wire().call(&[b"REPLICAOF" as &[u8], b"127.0.0.1", upstream.as_bytes()]);
        assert_eq!(reply, b"+OK\r\n");
        node
    }

    pub fn wire(&self) -> Wire {
        let s = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
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
