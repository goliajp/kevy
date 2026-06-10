//! Single-node cluster mode: per-shard deterministic ports, `-MOVED`
//! redirects on the cluster ports, full compat behaviour on the main port,
//! and the routing migration (KevyHash → slots reshard) being lossless.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

static START_GATE: Mutex<()> = Mutex::new(());

/// Pick a base port such that `base..=base+n` are all currently bindable
/// (the runtime needs the compat port plus `n` cluster ports). The probe
/// listeners are dropped before returning; START_GATE is held from here
/// until the runtime is up, closing the rebind race between tests.
fn free_port_block(n: usize) -> u16 {
    'retry: loop {
        let anchor = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = anchor.local_addr().unwrap().port();
        if base.checked_add(n as u16).is_none() {
            continue;
        }
        let mut probes = Vec::with_capacity(n);
        for i in 1..=n as u16 {
            match std::net::TcpListener::bind(("127.0.0.1", base + i)) {
                Ok(l) => probes.push(l),
                Err(_) => continue 'retry,
            }
        }
        return base;
    }
}

fn req(parts: &[&[u8]]) -> Vec<u8> {
    let mut v = format!("*{}\r\n", parts.len()).into_bytes();
    for p in parts {
        v.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
        v.extend_from_slice(p);
        v.extend_from_slice(b"\r\n");
    }
    v
}

fn read_reply(s: &mut std::net::TcpStream, expected: &[u8]) {
    let mut buf = vec![0u8; expected.len()];
    s.read_exact(&mut buf).unwrap();
    assert_eq!(
        &buf,
        expected,
        "expected {:?} got {:?}",
        String::from_utf8_lossy(expected),
        String::from_utf8_lossy(&buf),
    );
}

/// Read one CRLF-terminated line (for error replies of unknown length).
fn read_line(s: &mut std::net::TcpStream) -> String {
    let mut line = Vec::new();
    let mut b = [0u8; 1];
    loop {
        s.read_exact(&mut b).unwrap();
        line.push(b[0]);
        if line.ends_with(b"\r\n") {
            break;
        }
    }
    String::from_utf8(line).unwrap()
}

/// The contiguous-even-split inverse the server uses (16384 = 2^14).
fn slot_to_shard(slot: u16, n: usize) -> usize {
    (slot as usize * n) >> 14
}

struct Server {
    port: u16,
    cluster_base: u16,
    dir: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start(nshards: usize, cluster: bool, dir: Option<std::path::PathBuf>) -> Server {
        let _gate = START_GATE.lock().unwrap_or_else(|e| e.into_inner());
        let port = free_port_block(if cluster { nshards } else { 0 });
        let cluster_base = port + 1;
        let dir = dir.unwrap_or_else(|| {
            std::env::temp_dir().join(format!(
                "kevy-cluster-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ))
        });
        std::fs::create_dir_all(&dir).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let dir_thread = dir.clone();
        let handle = std::thread::spawn(move || {
            let mut rt = kevy_rt::Runtime::new([127, 0, 0, 1], port, nshards, kevy::KevyCommands)
                .with_data_dir(dir_thread);
            if cluster {
                rt = rt.with_cluster(cluster_base);
            }
            rt.run(stop_thread).unwrap();
        });
        // Wait until EVERY listener answers, not just the first: `connect`
        // on the compat port succeeds as soon as shard 0 has bound, while
        // shards 1..n are still binding their cluster ports — releasing the
        // START_GATE at that point lets the next test's port probe race
        // those in-flight binds (AddrInUse kills the runtime mid-start).
        let mut ports: Vec<u16> = vec![port];
        if cluster {
            ports.extend((0..nshards as u16).map(|i| cluster_base + i));
        }
        for p in ports {
            let mut ready = false;
            for _ in 0..400 {
                if std::net::TcpStream::connect(("127.0.0.1", p)).is_ok() {
                    ready = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert!(ready, "runtime did not come up on port {p}");
        }
        Server { port, cluster_base, dir, stop, handle: Some(handle) }
    }

    fn connect(&self) -> std::net::TcpStream {
        std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap()
    }

    fn connect_shard(&self, i: usize) -> std::net::TcpStream {
        std::net::TcpStream::connect(("127.0.0.1", self.cluster_base + i as u16)).unwrap()
    }

    /// Stop the runtime but keep the data dir (for reopen tests).
    fn shutdown_keep_dir(mut self) -> std::path::PathBuf {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        std::mem::take(&mut self.dir)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        if self.dir.as_os_str() != "" {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

/// Find a key whose slot routes to `want` under `n` shards.
fn key_for_shard(want: usize, n: usize) -> (String, u16) {
    for i in 0..100_000u32 {
        let key = format!("k{i}");
        let slot = kevy_hash::key_hash_slot(key.as_bytes());
        if slot_to_shard(slot, n) == want {
            return (key, slot);
        }
    }
    panic!("no key found for shard {want}/{n}");
}

#[test]
fn cluster_port_moves_wrong_shard_key_and_serves_own() {
    let n = 4;
    let srv = Server::start(n, true, None);

    // Connected to shard 0's cluster port: a shard-0 key works...
    let (own_key, _) = key_for_shard(0, n);
    let mut c0 = srv.connect_shard(0);
    c0.write_all(&req(&[b"SET", own_key.as_bytes(), b"v"])).unwrap();
    read_reply(&mut c0, b"+OK\r\n");

    // ...and a wrong-shard key gets -MOVED <slot> 127.0.0.1:<owner port>.
    let (remote_key, slot) = key_for_shard(2, n);
    c0.write_all(&req(&[b"SET", remote_key.as_bytes(), b"v"])).unwrap();
    let line = read_line(&mut c0);
    let want = format!("-MOVED {slot} 127.0.0.1:{}\r\n", srv.cluster_base + 2);
    assert_eq!(line, want);

    // The owner's cluster port serves that key directly.
    let mut c2 = srv.connect_shard(2);
    c2.write_all(&req(&[b"SET", remote_key.as_bytes(), b"v2"])).unwrap();
    read_reply(&mut c2, b"+OK\r\n");
}

#[test]
fn compat_port_keeps_forwarding_in_cluster_mode() {
    let srv = Server::start(4, true, None);
    let mut c = srv.connect();
    for i in 0..100u32 {
        let key = format!("compat{i}");
        c.write_all(&req(&[b"SET", key.as_bytes(), b"v"])).unwrap();
        read_reply(&mut c, b"+OK\r\n");
    }
    for i in 0..100u32 {
        let key = format!("compat{i}");
        c.write_all(&req(&[b"GET", key.as_bytes()])).unwrap();
        read_reply(&mut c, b"$1\r\nv\r\n");
    }
    // MGET across shards still fans out (superset vs Redis CROSSSLOT).
    c.write_all(&req(&[b"MGET", b"compat0", b"compat1", b"compat2"])).unwrap();
    read_reply(&mut c, b"*3\r\n$1\r\nv\r\n$1\r\nv\r\n$1\r\nv\r\n");
}

#[test]
fn cluster_slots_topology_is_exact_and_covering() {
    let n = 4;
    let srv = Server::start(n, true, None);
    // The CLUSTER command surface reads the process-wide config (normally
    // installed by `serve`); tests drive the Runtime directly, so install
    // one matching this server. First-init wins — the other tests in this
    // binary never read it.
    let mut cfg = kevy_config::Config::default();
    cfg.server.port = srv.port;
    cfg.server.threads = n;
    cfg.cluster.enabled = true;
    cfg.cluster.port_base = srv.cluster_base;
    kevy::config_init(std::sync::Arc::new(cfg));

    let mut c = srv.connect();
    c.write_all(&req(&[b"CLUSTER", b"SLOTS"])).unwrap();

    // Expected bytes are fully deterministic: 4 ranges of 4096 slots, ports
    // base..base+3, node ids 0...01 — 0...04, trailing empty metadata array.
    let mut want = String::from("*4\r\n");
    for i in 0..n {
        let start = i * 4096;
        let end = (i + 1) * 4096 - 1;
        want.push_str(&format!(
            "*3\r\n:{start}\r\n:{end}\r\n*4\r\n$9\r\n127.0.0.1\r\n:{}\r\n$40\r\n{:040x}\r\n*0\r\n",
            srv.cluster_base as usize + i,
            i + 1,
        ));
    }
    read_reply(&mut c, want.as_bytes());

    // INFO reports cluster_enabled:1 with 4 known nodes.
    c.write_all(&req(&[b"CLUSTER", b"INFO"])).unwrap();
    let mut buf = [0u8; 512];
    let got = c.read(&mut buf).unwrap();
    let text = String::from_utf8_lossy(&buf[..got]).to_string();
    assert!(text.contains("cluster_enabled:1"), "{text}");
    assert!(text.contains("cluster_known_nodes:4"), "{text}");
}

#[test]
fn keyslot_matches_local_computation() {
    let srv = Server::start(2, true, None);
    let mut c = srv.connect();
    for key in [&b"foo"[..], b"{user1000}.following", b"k42"] {
        c.write_all(&req(&[b"CLUSTER", b"KEYSLOT", key])).unwrap();
        let want = format!(":{}\r\n", kevy_hash::key_hash_slot(key));
        read_reply(&mut c, want.as_bytes());
    }
}

#[test]
fn reshard_migrates_kevyhash_data_to_slots_losslessly() {
    let n = 4;
    // Phase 1: plain (KevyHash-routed) server writes 200 keys, then SAVEs.
    let srv = Server::start(n, false, None);
    let mut c = srv.connect();
    for i in 0..200u32 {
        let key = format!("mig{i}");
        let val = format!("val{i}");
        c.write_all(&req(&[b"SET", key.as_bytes(), val.as_bytes()])).unwrap();
        read_reply(&mut c, b"+OK\r\n");
    }
    drop(c);
    let dir = srv.shutdown_keep_dir();

    // Phase 2: same dir reopened in cluster mode → startup reshard re-homes
    // every key under slot routing; all 200 stay readable (compat port).
    let srv2 = Server::start(n, true, Some(dir.clone()));
    let mut c2 = srv2.connect();
    for i in 0..200u32 {
        let key = format!("mig{i}");
        let want = format!("val{i}");
        c2.write_all(&req(&[b"GET", key.as_bytes()])).unwrap();
        read_reply(&mut c2, format!("${}\r\n{}\r\n", want.len(), want).as_bytes());
    }
    // And each key now lives on its slot-owner shard: the owner's cluster
    // port serves it without a redirect.
    let (probe, _) = key_for_shard(1, n);
    let mut c1 = srv2.connect_shard(1);
    c1.write_all(&req(&[b"SET", probe.as_bytes(), b"x"])).unwrap();
    read_reply(&mut c1, b"+OK\r\n");

    // Meta records the slots routing; sources were backed up.
    let meta = std::fs::read_to_string(dir.join("shards.meta")).unwrap();
    assert_eq!(meta, format!("{n}\nslots\n"));
    let backups = std::fs::read_dir(&dir)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".premigration.")
        })
        .count();
    assert!(backups > 0, "expected .premigration backups in {dir:?}");
}
