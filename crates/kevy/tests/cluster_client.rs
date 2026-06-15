//! Integration test for the cluster-aware `kevy_client::ClusterClient`: it
//! discovers the topology via CLUSTER SLOTS and routes every key to its owner
//! shard, so a workload spanning all shards never triggers `-MOVED`.
//!
//! In its own test binary (= its own process) because the CLUSTER command
//! surface reads the process-wide `config_global`, which is install-once —
//! sharing it with `cluster.rs`'s config-reading test would cross the wires.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use kevy_client::ClusterClient;

/// A free base port with `base..=base+n` all bindable (compat port + n cluster
/// ports). Hold the anchors until just before bind to avoid a TOCTOU race.
fn free_port_block(n: usize) -> u16 {
    loop {
        let anchor = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = anchor.local_addr().unwrap().port();
        let mut ok = true;
        let mut held = vec![anchor];
        for i in 1..=n as u16 {
            match std::net::TcpListener::bind(("127.0.0.1", base + i)) {
                Ok(l) => held.push(l),
                Err(_) => {
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            return base;
        }
    }
}

struct Server {
    cluster_base: u16,
    dir: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start(n: usize) -> Server {
        let port = free_port_block(n);
        let cluster_base = port + 1;
        let dir = std::env::temp_dir().join(format!(
            "kevy-cluster-client-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // CLUSTER SLOTS reads the process-wide config; install one matching
        // this server (own binary ⇒ first-init-wins is uncontested).
        let mut cfg = kevy_config::Config::default();
        cfg.server.port = port;
        cfg.server.threads = n;
        cfg.cluster.enabled = true;
        cfg.cluster.port_base = cluster_base;
        kevy::config_init(Arc::new(cfg));

        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let dir_thread = dir.clone();
        let handle = std::thread::spawn(move || {
            let rt = kevy_rt::Runtime::new([127, 0, 0, 1], port, n, kevy::KevyCommands)
                .with_data_dir(dir_thread)
                .with_cluster(cluster_base);
            rt.run(stop_thread).unwrap();
        });
        // Wait until every cluster listener answers.
        for p in (0..n as u16).map(|i| cluster_base + i) {
            let mut ready = false;
            for _ in 0..400 {
                if std::net::TcpStream::connect(("127.0.0.1", p)).is_ok() {
                    ready = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(ready, "runtime did not bind cluster port {p}");
        }
        Server { cluster_base, dir, stop, handle: Some(handle) }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn cluster_client_routes_every_key_to_owner() {
    let n = 4;
    let srv = Server::start(n);
    let mut cc = ClusterClient::connect("127.0.0.1", srv.cluster_base).unwrap();
    assert_eq!(cc.shard_count(), n, "discovered all shards via CLUSTER SLOTS");

    // 400 keys hash across all 4 shards; a routing bug would -MOVED → error.
    for i in 0..400u32 {
        let key = format!("k{i}");
        cc.set(key.as_bytes(), b"v").unwrap();
        assert_eq!(cc.get(key.as_bytes()).unwrap().as_deref(), Some(&b"v"[..]));
    }
    assert_eq!(cc.dbsize().unwrap(), 400, "DBSIZE summed across shards");

    // Full routed surface on a single key.
    cc.set(b"cnt", b"0").unwrap();
    assert_eq!(cc.incr(b"cnt").unwrap(), 1);
    assert_eq!(cc.incr_by(b"cnt", 41).unwrap(), 42);
    assert!(cc.expire(b"cnt", Duration::from_secs(100)).unwrap());
    assert!(cc.ttl_ms(b"cnt").unwrap() > 90_000);
    assert!(cc.persist(b"cnt").unwrap());
    assert_eq!(cc.ttl_ms(b"cnt").unwrap(), -1);
    cc.set_with_ttl(b"timed", b"x", Duration::from_secs(60)).unwrap();
    assert!(cc.ttl_ms(b"timed").unwrap() > 50_000);

    // Multi-key DEL/EXISTS route per key across shards and sum.
    assert_eq!(cc.exists(&[b"k0", b"k1", b"nope"]).unwrap(), 2);
    assert_eq!(cc.del(&[b"k0", b"k1", b"k2"]).unwrap(), 3);
    assert_eq!(cc.dbsize().unwrap(), 400 - 3 + 2); // -3 deleted; +cnt +timed

    cc.ping().unwrap();
    cc.flushall().unwrap();
    assert_eq!(cc.dbsize().unwrap(), 0, "FLUSHALL cleared every shard");
}
