//! v3-cluster Phase 1.C: replication listener accepts replica
//! handshake and replies `+ACK <offset>` over a real socket. Streaming
//! of live frames + acked offset tracking is a follow-up task (T1.14);
//! this test pins down the listener bind + handshake reply contract.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

static START_GATE: Mutex<()> = Mutex::new(());

/// Probe-bind `n` consecutive free ports starting at some random base
/// (compat port + n cluster ports + n replication ports). Same pattern
/// as the cluster-test free_port_block, generalised for any width.
fn free_port_block(width: usize) -> u16 {
    'retry: loop {
        let anchor = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = anchor.local_addr().unwrap().port();
        if base.checked_add(width as u16).is_none() {
            continue;
        }
        let mut probes = Vec::with_capacity(width);
        for i in 1..=width as u16 {
            match std::net::TcpListener::bind(("127.0.0.1", base + i)) {
                Ok(l) => probes.push(l),
                Err(_) => continue 'retry,
            }
        }
        return base;
    }
}

struct Server {
    #[allow(dead_code)]
    port: u16,
    replication_base: u16,
    nshards: usize,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    _dir: tempdir::TempDir,
}

/// Tiny stand-in for the `tempfile` crate (zero-dep workspace rule).
mod tempdir {
    use std::path::PathBuf;
    pub struct TempDir {
        path: PathBuf,
    }
    impl TempDir {
        pub fn new(label: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!("{label}-{nanos}"));
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }
        pub fn path(&self) -> &std::path::Path {
            &self.path
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

impl Server {
    fn start(nshards: usize) -> Server {
        let _gate = START_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        // compat port + n replication ports (no cluster mode here).
        let base = free_port_block(nshards);
        let port = base;
        let replication_base = base + 1;
        let dir = tempdir::TempDir::new("kevy-replication-test");
        let dir_path = dir.path().to_path_buf();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();

        // Force the epoll/kqueue reactor — replication listener on
        // io_uring is gated off in v1.18.0 (see Issue Ledger I2 in the
        // v3-cluster plan + the `Runtime::run` startup check).
        // SAFETY: integration test owns its own process state; setting
        // an env var here is safe since no other thread reads
        // KEVY_IO_URING in parallel.
        unsafe {
            std::env::set_var("KEVY_IO_URING", "0");
        }

        let handle = std::thread::spawn(move || {
            let rt = kevy_rt::Runtime::new(
                [127, 0, 0, 1],
                port,
                nshards,
                kevy::KevyCommands,
            )
            .with_data_dir(dir_path)
            .with_aof(false)
            .with_replication(true, 1024 * 1024)
            .with_replication_listener(replication_base);
            let _ = rt.run(stop_thread);
        });

        // Wait for both the compat port and every replication port
        // to answer. Same gating pattern as the cluster integration
        // test: a `connect` on the compat port succeeds the moment
        // shard 0 binds, but the replication ports may still be
        // mid-bind when START_GATE is released.
        let mut ports = vec![port];
        ports.extend((0..nshards as u16).map(|i| replication_base + i));
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
        Server {
            port,
            replication_base,
            nshards,
            stop,
            handle: Some(handle),
            _dir: dir,
        }
    }

    fn shutdown(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn replicate_from(offset: &str, id: &str) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"*5\r\n");
    for arg in [b"REPLICATE".as_slice(), b"FROM", offset.as_bytes(), b"ID", id.as_bytes()] {
        v.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
        v.extend_from_slice(arg);
        v.extend_from_slice(b"\r\n");
    }
    v
}

fn read_to_eof(s: &mut std::net::TcpStream) -> Vec<u8> {
    let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(2)));
    let mut out = Vec::new();
    let mut chunk = [0u8; 256];
    loop {
        match s.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    out
}

#[test]
fn replica_handshake_receives_ack_and_stays_connected() {
    // Post-T1.14: after `+ACK` the conn transitions to Streaming
    // (was Closed before). With no source mutations, the replica
    // just sees the +ACK and a quiet socket — `read_to_eof` returns
    // when its 2 s timeout elapses, NOT when the server closes.
    let server = Server::start(1);
    let mut s = std::net::TcpStream::connect(("127.0.0.1", server.replication_base)).unwrap();
    s.write_all(&replicate_from("0", "replica-a")).unwrap();
    let reply = read_to_eof(&mut s);
    assert_eq!(
        reply, b"+ACK 0\r\n",
        "got {:?}",
        String::from_utf8_lossy(&reply),
    );
    server.shutdown();
}

#[test]
fn handshake_with_nonzero_offset_echoed_in_ack() {
    let server = Server::start(1);
    let mut s = std::net::TcpStream::connect(("127.0.0.1", server.replication_base)).unwrap();
    s.write_all(&replicate_from("12345", "node-7")).unwrap();
    let reply = read_to_eof(&mut s);
    assert_eq!(
        reply, b"+ACK 12345\r\n",
        "got {:?}",
        String::from_utf8_lossy(&reply),
    );
    server.shutdown();
}

#[test]
fn malformed_handshake_closes_connection_no_ack() {
    let server = Server::start(1);
    let mut s = std::net::TcpStream::connect(("127.0.0.1", server.replication_base)).unwrap();
    // Send PING instead of REPLICATE FROM ... — the handshake rejects
    // and the server drops the conn without writing a reply.
    s.write_all(b"*1\r\n$4\r\nPING\r\n").unwrap();
    let reply = read_to_eof(&mut s);
    assert!(reply.is_empty(), "got unexpected reply {reply:?}");
    server.shutdown();
}

#[test]
fn replication_disabled_means_no_listener_on_replication_port() {
    // Spin up a server WITHOUT replication and confirm the would-be
    // replication port is NOT bound. This guards against a wiring
    // mistake that always binds the listener regardless of config.
    let _gate = START_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let base = free_port_block(1);
    let dir = tempdir::TempDir::new("kevy-replication-disabled");
    let dir_path = dir.path().to_path_buf();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    // SAFETY: see other set_var call in this file — single test thread.
    unsafe {
        std::env::set_var("KEVY_IO_URING", "0");
    }
    let handle = std::thread::spawn(move || {
        let rt = kevy_rt::Runtime::new([127, 0, 0, 1], base, 1, kevy::KevyCommands)
            .with_data_dir(dir_path)
            .with_aof(false);
        // No .with_replication / .with_replication_listener calls.
        let _ = rt.run(stop_thread);
    });
    // Wait for compat port.
    for _ in 0..400 {
        if std::net::TcpStream::connect(("127.0.0.1", base)).is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    // Replication port range would conventionally be base + 10000 + 0,
    // but here we just verify the default-disabled state by trying
    // base + 1 (cluster slot) — it should also be unbound. Use a
    // 50 ms timeout connect so the test doesn't hang on platforms
    // where unconnected TCP returns CONNREFUSED slowly.
    let addr: std::net::SocketAddr = format!("127.0.0.1:{}", base + 1).parse().unwrap();
    let connect = std::net::TcpStream::connect_timeout(
        &addr,
        std::time::Duration::from_millis(100),
    );
    assert!(
        connect.is_err(),
        "no listener should be on the would-be replication port without with_replication_listener",
    );
    stop.store(true, Ordering::Relaxed);
    let _ = handle.join();
}

/// Read at least `min` bytes (or until EOF / 5 s) from a socket.
/// Used by the streaming tests where we need to wait until the
/// primary actually pushes a frame.
fn read_at_least(s: &mut std::net::TcpStream, min: usize) -> Vec<u8> {
    let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(5)));
    let mut out = Vec::new();
    let mut chunk = [0u8; 1024];
    while out.len() < min {
        match s.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    out
}

fn send_resp(s: &mut std::net::TcpStream, parts: &[&[u8]]) {
    let mut v = format!("*{}\r\n", parts.len()).into_bytes();
    for p in parts {
        v.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
        v.extend_from_slice(p);
        v.extend_from_slice(b"\r\n");
    }
    s.write_all(&v).unwrap();
}

fn read_line(s: &mut std::net::TcpStream) -> Vec<u8> {
    let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(2)));
    let mut line = Vec::new();
    let mut b = [0u8; 1];
    loop {
        s.read_exact(&mut b).unwrap();
        line.push(b[0]);
        if line.ends_with(b"\r\n") {
            return line;
        }
    }
}

#[test]
fn streaming_replica_receives_set_command_as_wire_frame() {
    // Single-shard server so every SET lands on the only backlog.
    let server = Server::start(1);

    // Replica connects to the replication port and handshakes from 0.
    let mut replica = std::net::TcpStream::connect((
        "127.0.0.1",
        server.replication_base,
    ))
    .unwrap();
    replica.write_all(&replicate_from("0", "replica-stream")).unwrap();
    // First bytes back must be the +ACK.
    let ack = read_at_least(&mut replica, b"+ACK 0\r\n".len());
    assert!(ack.starts_with(b"+ACK 0\r\n"), "got {:?}", String::from_utf8_lossy(&ack));

    // Now a regular client on the main port issues a SET.
    let mut client = std::net::TcpStream::connect(("127.0.0.1", server.port)).unwrap();
    send_resp(&mut client, &[b"SET", b"foo", b"bar"]);
    let ok = read_line(&mut client);
    assert_eq!(ok, b"+OK\r\n");

    // Replica should receive the frame. The +ACK may or may not have
    // been fully consumed by `read_at_least`; pull the leftover bytes
    // (everything after the ACK we already saw).
    let mut buf = ack[b"+ACK 0\r\n".len()..].to_vec();
    while buf.is_empty() || !buf.windows(2).any(|w| w == b"ar") {
        let mut chunk = [0u8; 256];
        match replica.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
        if buf.len() > 4096 {
            break;
        }
    }
    let (offset, argv, used) =
        kevy_replicate::wire::decode_frame(&buf).expect("decode frame");
    assert_eq!(offset, 0);
    assert_eq!(argv.len(), 3);
    assert_eq!(argv.get(0), Some(&b"SET"[..]));
    assert_eq!(argv.get(1), Some(&b"foo"[..]));
    assert_eq!(argv.get(2), Some(&b"bar"[..]));
    assert!(used <= buf.len());

    server.shutdown();
}

#[test]
fn streaming_replica_receives_multiple_frames_in_order() {
    let server = Server::start(1);
    let mut replica = std::net::TcpStream::connect((
        "127.0.0.1",
        server.replication_base,
    ))
    .unwrap();
    replica.write_all(&replicate_from("0", "replica-multi")).unwrap();
    let ack = read_at_least(&mut replica, b"+ACK 0\r\n".len());
    assert!(ack.starts_with(b"+ACK 0\r\n"));

    let mut client = std::net::TcpStream::connect(("127.0.0.1", server.port)).unwrap();
    for i in 0..5 {
        send_resp(&mut client, &[b"SET", format!("k{i}").as_bytes(), format!("v{i}").as_bytes()]);
        let ok = read_line(&mut client);
        assert_eq!(ok, b"+OK\r\n");
    }

    // Collect bytes after the ACK until we have 5 decoded frames.
    let mut buf = ack[b"+ACK 0\r\n".len()..].to_vec();
    let mut frames: Vec<(u64, kevy_resp::Argv)> = Vec::new();
    let mut cursor = 0usize;
    while frames.len() < 5 {
        if buf.len() - cursor > 0 {
            match kevy_replicate::wire::decode_frame(&buf[cursor..]) {
                Ok((offset, argv, used)) => {
                    frames.push((offset, argv));
                    cursor += used;
                    continue;
                }
                Err(kevy_replicate::wire::WireError::Truncated) => {
                    // need more bytes
                }
                Err(e) => panic!("decode error: {e}"),
            }
        }
        let mut chunk = [0u8; 256];
        let n = match replica.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > 65536 {
            break;
        }
    }
    assert_eq!(frames.len(), 5, "expected 5 frames, got {}", frames.len());
    for (i, (offset, argv)) in frames.iter().enumerate() {
        assert_eq!(*offset, i as u64, "frame {i} offset");
        assert_eq!(argv.get(0), Some(&b"SET"[..]));
        assert_eq!(argv.get(1), Some(format!("k{i}").as_bytes()));
        assert_eq!(argv.get(2), Some(format!("v{i}").as_bytes()));
    }
    server.shutdown();
}

#[test]
fn streaming_replica_receives_only_its_shards_writes() {
    // 2-shard server. SETs are key-routed: "alpha" and "beta" likely
    // land on different shards (kevy_hash). A replica on shard 0
    // should only see writes whose key routes to shard 0; same for
    // shard 1.
    let server = Server::start(2);
    let mut replicas: Vec<_> = (0..server.nshards)
        .map(|i| {
            let mut r = std::net::TcpStream::connect((
                "127.0.0.1",
                server.replication_base + i as u16,
            ))
            .unwrap();
            r.write_all(&replicate_from("0", &format!("replica-{i}"))).unwrap();
            let ack = read_at_least(&mut r, b"+ACK 0\r\n".len());
            assert!(ack.starts_with(b"+ACK 0\r\n"));
            (r, ack)
        })
        .collect();

    let mut client = std::net::TcpStream::connect(("127.0.0.1", server.port)).unwrap();
    // Write several keys; we expect them split across shards.
    let keys = ["alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta"];
    for k in keys {
        send_resp(&mut client, &[b"SET", k.as_bytes(), b"v"]);
        let ok = read_line(&mut client);
        assert_eq!(ok, b"+OK\r\n");
    }

    // Total frames across both replicas must equal the number of SETs.
    let mut total_received = 0usize;
    let mut all_keys: Vec<Vec<u8>> = Vec::new();
    for (r, ack) in &mut replicas {
        let mut buf = ack[b"+ACK 0\r\n".len()..].to_vec();
        let mut cursor = 0usize;
        let _ = r.set_read_timeout(Some(std::time::Duration::from_millis(500)));
        loop {
            // Try to decode out of what's buffered.
            match kevy_replicate::wire::decode_frame(&buf[cursor..]) {
                Ok((_, argv, used)) => {
                    cursor += used;
                    total_received += 1;
                    all_keys.push(argv.get(1).unwrap().to_vec());
                    continue;
                }
                Err(kevy_replicate::wire::WireError::Truncated) => {}
                Err(e) => panic!("decode: {e}"),
            }
            let mut chunk = [0u8; 256];
            match r.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(_) => break,
            }
            if buf.len() > 65536 {
                break;
            }
        }
    }
    assert_eq!(
        total_received,
        keys.len(),
        "expected {} frames across both shards, got {}",
        keys.len(),
        total_received,
    );
    // Every key must appear exactly once.
    all_keys.sort();
    let mut expected: Vec<Vec<u8>> = keys.iter().map(|k| k.as_bytes().to_vec()).collect();
    expected.sort();
    assert_eq!(all_keys, expected);
    server.shutdown();
}

#[test]
fn replica_client_handshake_and_receive_set_frame() {
    // The "real" replica path: kevy_replicate::replica::ReplicaClient
    // does the handshake + frame decoding for the caller. Mirror the
    // ad-hoc SET test, but via the published replica API instead of
    // raw TCP — pins down the client contract end-to-end.
    let server = Server::start(1);
    let mut client = kevy_replicate::replica::ReplicaClient::connect(
        ("127.0.0.1", server.replication_base),
        "replica-via-client",
        0,
    )
    .expect("connect + handshake");
    assert_eq!(client.primary_offset_at_handshake(), 0);
    assert_eq!(client.expected_offset(), 0);

    // Run a SET via the main port.
    let mut writer = std::net::TcpStream::connect(("127.0.0.1", server.port)).unwrap();
    send_resp(&mut writer, &[b"SET", b"foo", b"bar"]);
    let ok = read_line(&mut writer);
    assert_eq!(ok, b"+OK\r\n");

    // Iterator yields the frame.
    let frame = client.next().expect("frame").expect("decode ok");
    assert_eq!(frame.offset, 0);
    assert_eq!(frame.argv.len(), 3);
    assert_eq!(frame.argv.get(0), Some(&b"SET"[..]));
    assert_eq!(frame.argv.get(1), Some(&b"foo"[..]));
    assert_eq!(frame.argv.get(2), Some(&b"bar"[..]));
    // After consuming offset 0, expected_offset advances to 1.
    assert_eq!(client.expected_offset(), 1);

    drop(client);
    server.shutdown();
}

#[test]
fn replica_client_handshake_failure_on_closed_port() {
    // No server running on this port — connect should fail.
    // Use a port we just released (probe-and-drop) so it's almost
    // certainly unbound, with a short timeout so the test is quick.
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    let result = kevy_replicate::replica::ReplicaClient::connect_with_timeout(
        ("127.0.0.1", port),
        "replica-x",
        0,
        std::time::Duration::from_millis(200),
    );
    assert!(
        result.is_err(),
        "connect to released port should fail, got Ok",
    );
}

/// Spawn a primary with a small replication buffer so backlog
/// eviction kicks in after just a few writes. Used by the snapshot-
/// ship test below to force the TooOld path.
fn start_small_buffer_primary(buffer_size: u64) -> Server {
    let _gate = START_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let base = free_port_block(1);
    let port = base;
    let replication_base = base + 1;
    let dir = tempdir::TempDir::new("kevy-snapshot-ship");
    let dir_path = dir.path().to_path_buf();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    // SAFETY: see Server::start.
    unsafe {
        std::env::set_var("KEVY_IO_URING", "0");
    }
    let handle = std::thread::spawn(move || {
        let rt = kevy_rt::Runtime::new([127, 0, 0, 1], port, 1, kevy::KevyCommands)
            .with_data_dir(dir_path)
            .with_aof(false)
            .with_replication(true, buffer_size)
            .with_replication_listener(replication_base);
        let _ = rt.run(stop_thread);
    });
    for p in [port, replication_base] {
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
    Server {
        port,
        replication_base,
        nshards: 1,
        stop,
        handle: Some(handle),
        _dir: dir,
    }
}

#[test]
fn snapshot_ship_triggers_when_replica_falls_behind_backlog() {
    use kevy_replicate::replica::{ReplicaClient, ReplicaEvent};

    // T1.23: a replica that asks for `from_offset = 0` after the
    // primary's backlog has evicted offset 0 triggers a snapshot
    // ship. Verify the full sequence: SnapshotBegin → ≥ 1 Chunk →
    // SnapshotEnd { ack_offset } → expected_offset advances to
    // ack_offset (no gap when live frames resume).

    // Tiny buffer — each frame ~37 B, so 256 B holds ~7 frames. We
    // write 30 keys so offsets 0..~23 are evicted, forcing TooOld
    // when the replica asks for offset 0.
    let server = start_small_buffer_primary(256);

    let mut writer = std::net::TcpStream::connect(("127.0.0.1", server.port)).unwrap();
    for i in 0..30 {
        send_resp(&mut writer, &[b"SET", format!("k{i}").as_bytes(), b"v"]);
        let ok = read_line(&mut writer);
        assert_eq!(ok, b"+OK\r\n");
    }

    let mut client = ReplicaClient::connect(
        ("127.0.0.1", server.replication_base),
        "replica-snapshot",
        0,
    )
    .expect("connect + handshake");

    // First event must be SnapshotBegin (TooOld → snapshot ship).
    match client.next_event().expect("event").expect("ok") {
        ReplicaEvent::SnapshotBegin => {}
        other => panic!("expected SnapshotBegin, got {other:?}"),
    }

    // Accumulate chunks until SnapshotEnd.
    let mut snapshot_bytes = Vec::new();
    let ack_offset = loop {
        match client.next_event().expect("event").expect("ok") {
            ReplicaEvent::SnapshotChunk(bytes) => snapshot_bytes.extend(bytes),
            ReplicaEvent::SnapshotEnd { ack_offset } => break ack_offset,
            other => panic!("expected SnapshotChunk or SnapshotEnd, got {other:?}"),
        }
    };

    // Snapshot ack_offset == primary's next_offset at trigger time;
    // we wrote 30 SETs so primary's next_offset is 30. The snapshot's
    // ack_offset must equal that — replica jumps expected_offset
    // there, future live frames will arrive at 30.
    assert_eq!(ack_offset, 30, "ack_offset");
    assert_eq!(client.expected_offset(), 30);

    // Snapshot bytes start with kevy_persist's RDB MAGIC (`KEVYSNAP`).
    // Just check the prefix — a full load_snapshot round-trip is T1.24.
    assert!(snapshot_bytes.len() > 8, "snapshot too small");
    assert_eq!(&snapshot_bytes[..8], b"KEVYSNAP", "snapshot magic");

    drop(client);
    server.shutdown();
}

#[test]
fn snapshot_ship_loaded_into_local_store_matches_primary() {
    use kevy_replicate::replica::{ReplicaClient, ReplicaEvent};

    // T1.24: full primary→replica round-trip via snapshot ship.
    // Primary writes N keys, backlog evicts so replica falls behind,
    // primary ships a snapshot, replica loads it into a fresh local
    // store via kevy_persist::load_snapshot_from, and GET on the
    // local store returns byte-equivalent values to what the primary
    // stored. Proves the snapshot path closes the loop.

    let server = start_small_buffer_primary(256);

    // Stage N writes against the primary; backlog evicts old offsets.
    let pairs: Vec<(String, String)> = (0..20)
        .map(|i| (format!("snap-k{i}"), format!("val-{i:04}")))
        .collect();
    let mut writer = std::net::TcpStream::connect(("127.0.0.1", server.port)).unwrap();
    for (k, v) in &pairs {
        send_resp(&mut writer, &[b"SET", k.as_bytes(), v.as_bytes()]);
        let ok = read_line(&mut writer);
        assert_eq!(ok, b"+OK\r\n");
    }

    // Replica connects from 0; primary detects TooOld + ships snapshot.
    let mut client = ReplicaClient::connect(
        ("127.0.0.1", server.replication_base),
        "replica-loader",
        0,
    )
    .expect("connect");
    assert!(matches!(
        client.next_event().expect("event").expect("ok"),
        ReplicaEvent::SnapshotBegin
    ));
    let mut snapshot_bytes = Vec::new();
    let ack_offset = loop {
        match client.next_event().expect("event").expect("ok") {
            ReplicaEvent::SnapshotChunk(bytes) => snapshot_bytes.extend(bytes),
            ReplicaEvent::SnapshotEnd { ack_offset } => break ack_offset,
            other => panic!("unexpected event: {other:?}"),
        }
    };
    assert_eq!(ack_offset, pairs.len() as u64);

    // Load the streamed snapshot into a fresh local Store via the new
    // `load_snapshot_from` API (T1.24). The Store is the primitive
    // single-shard kevy_store::Store; for multi-shard replicas the
    // caller routes by hash before load. Single-shard is enough here
    // to prove the contract.
    let mut local_store = kevy_store::Store::new();
    kevy_persist::load_snapshot_from(&mut local_store, std::io::Cursor::new(&snapshot_bytes))
        .expect("load_snapshot_from");

    // GET each primary-written key against the loaded local store
    // and verify byte-equivalence. Uses kevy::dispatch (same path
    // T1.19's in-process apply recipe used).
    for (k, v) in &pairs {
        let argv = kevy::Argv::from(vec![b"GET".to_vec(), k.as_bytes().to_vec()]);
        let reply = kevy::dispatch(&mut local_store, &argv);
        let expected = format!("${}\r\n{}\r\n", v.len(), v);
        assert_eq!(
            reply, expected.as_bytes(),
            "key {k:?}: loaded replica returned {:?}, expected {:?}",
            String::from_utf8_lossy(&reply),
            expected,
        );
    }

    drop(client);
    server.shutdown();
}

#[test]
fn fresh_replica_join_snapshot_then_live_frames() {
    use kevy_replicate::replica::{ReplicaClient, ReplicaEvent};

    // T1.27: Phase 1.E e2e. A fresh replica joins a primary whose
    // backlog has already evicted offset 0 → it takes the snapshot
    // path; after `SnapshotEnd { ack_offset }` the replica receives
    // post-snapshot live frames at offsets `ack_offset..` with no
    // gap. Proves the snapshot→live transition closes the full
    // primary→replica round-trip — both halves applied to a single
    // local store produce byte-equivalent GETs for every key.

    let server = start_small_buffer_primary(256);

    // Stage 1: pre-snapshot writes overflow the 256 B backlog, so a
    // from-0 replica will trigger snapshot ship.
    let pre: Vec<(String, String)> = (0..20)
        .map(|i| (format!("pre-k{i}"), format!("pre-v{i:04}")))
        .collect();
    let mut writer = std::net::TcpStream::connect(("127.0.0.1", server.port)).unwrap();
    for (k, v) in &pre {
        send_resp(&mut writer, &[b"SET", k.as_bytes(), v.as_bytes()]);
        assert_eq!(read_line(&mut writer), b"+OK\r\n");
    }

    // Stage 2: replica connects from 0, drains the snapshot path.
    let mut client = ReplicaClient::connect(
        ("127.0.0.1", server.replication_base),
        "replica-t127",
        0,
    )
    .expect("connect + handshake");
    assert!(matches!(
        client.next_event().expect("event").expect("ok"),
        ReplicaEvent::SnapshotBegin
    ));
    let mut snapshot_bytes = Vec::new();
    let ack_offset = loop {
        match client.next_event().expect("event").expect("ok") {
            ReplicaEvent::SnapshotChunk(bytes) => snapshot_bytes.extend(bytes),
            ReplicaEvent::SnapshotEnd { ack_offset } => break ack_offset,
            other => panic!("expected SnapshotChunk or SnapshotEnd, got {other:?}"),
        }
    };
    assert_eq!(ack_offset, pre.len() as u64);
    assert_eq!(client.expected_offset(), ack_offset);

    // Stage 3: load the snapshot into a fresh local store.
    let mut local_store = kevy_store::Store::new();
    kevy_persist::load_snapshot_from(&mut local_store, std::io::Cursor::new(&snapshot_bytes))
        .expect("load_snapshot_from");

    // Stage 4: primary takes M post-snapshot writes; they arrive at
    // offsets `ack_offset..ack_offset+M`. M kept small so the 256 B
    // backlog holds the burst without re-evicting under the replica.
    let post: Vec<(String, String)> = (0..5)
        .map(|i| (format!("post-k{i}"), format!("post-v{i:04}")))
        .collect();
    for (k, v) in &post {
        send_resp(&mut writer, &[b"SET", k.as_bytes(), v.as_bytes()]);
        assert_eq!(read_line(&mut writer), b"+OK\r\n");
    }

    // Stage 5: drain M live Frame events with monotonic offsets
    // starting at `ack_offset`; apply each via `kevy::dispatch` into
    // the same local store loaded from the snapshot.
    for (i, _) in post.iter().enumerate() {
        let expected_offset = ack_offset + i as u64;
        match client.next_event().expect("event").expect("ok") {
            ReplicaEvent::Frame(frame) => {
                assert_eq!(
                    frame.offset, expected_offset,
                    "live frame {i}: offset mismatch (post-snapshot gap)",
                );
                let _ = kevy::dispatch(&mut local_store, &frame.argv);
            }
            other => panic!("live frame {i}: expected Frame, got {other:?}"),
        }
    }

    // Stage 6: every key — snapshot-loaded and live-frame-applied —
    // GETs byte-equivalent on the local store. That's the contract.
    for (k, v) in pre.iter().chain(post.iter()) {
        let argv = kevy::Argv::from(vec![b"GET".to_vec(), k.as_bytes().to_vec()]);
        let reply = kevy::dispatch(&mut local_store, &argv);
        let expected = format!("${}\r\n{}\r\n", v.len(), v);
        assert_eq!(
            reply, expected.as_bytes(),
            "key {k:?}: got {:?}, expected {:?}",
            String::from_utf8_lossy(&reply),
            expected,
        );
    }

    drop(client);
    server.shutdown();
}

#[test]
fn replica_apply_dispatch_mirrors_primary_store() {
    // T1.19: prove the apply path. After streaming N writes from
    // primary to a local in-process KeyspaceStore via kevy::dispatch,
    // GET on the local store returns byte-equivalent values to GET
    // on the primary. That's the full replication contract for the
    // in-process recipe.
    let server = Server::start(1);
    let mut client = kevy_replicate::replica::ReplicaClient::connect(
        ("127.0.0.1", server.replication_base),
        "replica-apply",
        0,
    )
    .expect("connect + handshake");

    // Issue a handful of mixed writes against the primary.
    let mut writer = std::net::TcpStream::connect(("127.0.0.1", server.port)).unwrap();
    let pairs: &[(&[u8], &[u8])] = &[
        (b"alpha", b"one"),
        (b"beta", b"two"),
        (b"gamma", b"three"),
        (b"delta", b"four"),
    ];
    for (k, v) in pairs {
        send_resp(&mut writer, &[b"SET", k, v]);
        let ok = read_line(&mut writer);
        assert_eq!(ok, b"+OK\r\n");
    }

    // Pull frames + apply via kevy::dispatch into a fresh local store.
    let mut local_store = kevy::KeyspaceStore::new();
    for expected in 0..pairs.len() as u64 {
        let frame = client.next().expect("frame").expect("decode ok");
        assert_eq!(frame.offset, expected);
        let _reply = kevy::dispatch(&mut local_store, &frame.argv);
    }

    // For every key written to primary, GET on the local replica
    // store returns byte-equivalent value. This is the contract:
    // applied(primary) == applied(replica).
    for (k, v) in pairs {
        let argv = kevy::Argv::from(vec![b"GET".to_vec(), k.to_vec()]);
        let reply = kevy::dispatch(&mut local_store, &argv);
        let expected = format!("${}\r\n{}\r\n", v.len(), String::from_utf8_lossy(v));
        assert_eq!(
            reply, expected.as_bytes(),
            "key {:?}: replica GET returned {:?}, expected {:?}",
            String::from_utf8_lossy(k),
            String::from_utf8_lossy(&reply),
            expected,
        );
    }

    drop(client);
    server.shutdown();
}

#[test]
fn role_reports_master_offset_advancing_with_writes() {
    // T1.28: `ROLE` on a primary returns `["master", <offset>, []]`
    // where <offset> tracks the shard's replication source. After
    // N writes the offset published per tick (~100 ms) should reflect
    // the writes — verify via the wire-level ROLE reply.

    let server = Server::start(1);
    let mut s = std::net::TcpStream::connect(("127.0.0.1", server.port)).unwrap();

    // Before any writes — first poll within tick interval. Wait for
    // the publish loop to fire at least once + observe ROLE reply.
    let mut last = Vec::new();
    for _ in 0..40 {
        send_resp(&mut s, &[b"ROLE"]);
        last = read_line_array(&mut s);
        if last.starts_with(b"*3\r\n") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        last.starts_with(b"*3\r\n$6\r\nmaster\r\n:0\r\n*0\r\n"),
        "initial ROLE expected master 0 empty; got {:?}",
        String::from_utf8_lossy(&last),
    );

    // Drive 7 writes against the primary.
    for i in 0..7 {
        send_resp(&mut s, &[b"SET", format!("rk{i}").as_bytes(), b"v"]);
        assert_eq!(read_line(&mut s), b"+OK\r\n");
    }

    // The ROLE offset is published by the per-tick view (default 100
    // ms). Poll up to ~1 s until the offset reflects the 7 writes.
    let mut saw_offset = 0u64;
    for _ in 0..100 {
        send_resp(&mut s, &[b"ROLE"]);
        let reply = read_line_array(&mut s);
        if let Some(off) = parse_role_master_offset(&reply) {
            saw_offset = off;
            if off >= 7 {
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(saw_offset, 7, "ROLE offset should reflect 7 writes");

    server.shutdown();
}

/// Read one RESP reply (one top-level message, possibly multi-line) by
/// reading until we've seen a complete `*N` array — used only by the
/// ROLE test where the reply is always `*3` or `*5`.
fn read_line_array(s: &mut std::net::TcpStream) -> Vec<u8> {
    let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(2)));
    // ROLE master = 23 bytes for the empty-array case; master+offset
    // tops out under 64 B for our test cardinality (offset ≤ 7).
    // Just pull whatever's available in one short window.
    let mut buf = vec![0u8; 256];
    match s.read(&mut buf) {
        Ok(n) => buf[..n].to_vec(),
        Err(_) => Vec::new(),
    }
}

/// Parse the integer at position 2 of a `ROLE` master reply
/// (`*3\r\n$6\r\nmaster\r\n:<N>\r\n*0\r\n`). Returns `None` for any
/// other shape.
fn parse_role_master_offset(reply: &[u8]) -> Option<u64> {
    let prefix = b"*3\r\n$6\r\nmaster\r\n:";
    if !reply.starts_with(prefix) {
        return None;
    }
    let rest = &reply[prefix.len()..];
    let end = rest.iter().position(|&b| b == b'\r')?;
    std::str::from_utf8(&rest[..end]).ok()?.parse().ok()
}

#[test]
fn multi_shard_listener_binds_per_shard_port() {
    // With nshards=3 each shard binds replication_base + i. Connect to
    // each independently and run a handshake; all should ACK.
    let server = Server::start(3);
    for i in 0..server.nshards {
        let port = server.replication_base + i as u16;
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.write_all(&replicate_from("0", &format!("replica-{i}"))).unwrap();
        let reply = read_to_eof(&mut s);
        assert_eq!(
            reply, b"+ACK 0\r\n",
            "shard {i} port {port}: got {:?}",
            String::from_utf8_lossy(&reply),
        );
    }
    server.shutdown();
}

/// T1.29(b)+(c)+(d) end-to-end: a SECOND kevy_rt::Runtime spun up as
/// a replica (via `with_replica_inboxes`) receives frames from the
/// primary via a manually-spawned runner thread and ends up with a
/// byte-equivalent keyspace. Validates the full pipe — replica
/// runner → ReplicaInboxSender → Shard.drain_replica_inbox →
/// apply_replica_frame (under `ReplicatedApplyGuard`) → local store.
struct ReplicaServer {
    port: u16,
    stop_runtime: Arc<AtomicBool>,
    stop_runner: Arc<AtomicBool>,
    rt_handle: Option<std::thread::JoinHandle<()>>,
    runner_handle: Option<std::thread::JoinHandle<()>>,
    _dir: tempdir::TempDir,
}

impl ReplicaServer {
    fn start(upstream_replication_port: u16) -> Self {
        let _gate = START_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let port = free_port_block(0) + 1; // one free port (no replication listener / cluster)
        let dir = tempdir::TempDir::new("kevy-replica-rt");
        let dir_path = dir.path().to_path_buf();
        // SAFETY: see Server::start.
        unsafe {
            std::env::set_var("KEVY_IO_URING", "0");
        }

        // One inbox pair (single-shard test).
        let (sender, receiver) = kevy_rt::replica_inbox_pair();

        let stop_runtime = Arc::new(AtomicBool::new(false));
        let stop_runtime_thread = stop_runtime.clone();
        let rt_handle = std::thread::spawn(move || {
            let rt = kevy_rt::Runtime::new(
                [127, 0, 0, 1],
                port,
                1,
                kevy::KevyCommands,
            )
            .with_data_dir(dir_path)
            .with_aof(false)
            .with_replica_inboxes(vec![receiver]);
            let _ = rt.run(stop_runtime_thread);
        });
        for _ in 0..400 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        // Manual replica runner — connects to the primary, forwards
        // every event into the inbox until told to stop.
        let stop_runner = Arc::new(AtomicBool::new(false));
        let stop_runner_thread = stop_runner.clone();
        let runner_handle = std::thread::spawn(move || {
            let mut from_offset: u64 = 0;
            while !stop_runner_thread.load(std::sync::atomic::Ordering::Relaxed) {
                let conn = kevy_replicate::replica::ReplicaClient::connect(
                    ("127.0.0.1", upstream_replication_port),
                    "test-runner",
                    from_offset,
                );
                let Ok(mut client) = conn else {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    continue;
                };
                while !stop_runner_thread.load(std::sync::atomic::Ordering::Relaxed) {
                    match client.next_event() {
                        Some(Ok(ev)) => {
                            let apply = match ev {
                                kevy_replicate::replica::ReplicaEvent::SnapshotBegin => {
                                    kevy_rt::ReplicaApply::SnapshotBegin
                                }
                                kevy_replicate::replica::ReplicaEvent::SnapshotChunk(b) => {
                                    kevy_rt::ReplicaApply::SnapshotChunk(b)
                                }
                                kevy_replicate::replica::ReplicaEvent::SnapshotEnd { ack_offset } => {
                                    from_offset = ack_offset;
                                    kevy_rt::ReplicaApply::SnapshotEnd { ack_offset }
                                }
                                kevy_replicate::replica::ReplicaEvent::Frame(frame) => {
                                    from_offset = frame.offset.saturating_add(1);
                                    kevy_rt::ReplicaApply::Frame {
                                        offset: frame.offset,
                                        argv: frame.argv,
                                    }
                                }
                            };
                            if sender.send(apply).is_err() {
                                return;
                            }
                        }
                        Some(Err(_)) | None => break,
                    }
                }
            }
        });

        Self {
            port,
            stop_runtime,
            stop_runner,
            rt_handle: Some(rt_handle),
            runner_handle: Some(runner_handle),
            _dir: dir,
        }
    }

    fn shutdown(mut self) {
        self.stop_runner.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(h) = self.runner_handle.take() {
            // Runner's blocking next_event may not unwind immediately;
            // dropping the sender ensures the shard side eventually sees
            // the channel close. Best-effort join.
            let _ = h.join();
        }
        self.stop_runtime.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(h) = self.rt_handle.take() {
            let _ = h.join();
        }
    }
}

#[test]
fn server_as_replica_applies_upstream_writes() {
    // Primary on its own Runtime + replica on a second Runtime in the
    // same process. Runner thread bridges them. Primary's writes
    // (a few SETs that fit in the default backlog) should land in the
    // replica's Store within a few ticks, queryable via the replica's
    // compat port.
    let primary = Server::start(1);

    // Write 5 keys to the primary; they enter its backlog at offsets
    // 0..5.
    let mut writer = std::net::TcpStream::connect(("127.0.0.1", primary.port)).unwrap();
    let pairs: &[(&[u8], &[u8])] = &[
        (b"alpha", b"one"),
        (b"beta", b"two"),
        (b"gamma", b"three"),
        (b"delta", b"four"),
        (b"epsilon", b"five"),
    ];
    for (k, v) in pairs {
        send_resp(&mut writer, &[b"SET", k, v]);
        assert_eq!(read_line(&mut writer), b"+OK\r\n");
    }

    // Bring up replica + runner pointing at primary's shard 0.
    let replica = ReplicaServer::start(primary.replication_base);

    // Poll the replica until all 5 keys are visible (or timeout).
    let mut reader = std::net::TcpStream::connect(("127.0.0.1", replica.port)).unwrap();
    let mut all_seen = false;
    for _ in 0..200 {
        let mut got_all = true;
        for (k, _v) in pairs {
            send_resp(&mut reader, &[b"GET", k]);
            let line = read_line(&mut reader);
            if line.starts_with(b"$-1") || line.starts_with(b"$0") {
                got_all = false;
                break;
            }
            if line.starts_with(b"$") {
                // bulk header — consume the payload line + crlf
                let _ = read_line(&mut reader);
            }
        }
        if got_all {
            all_seen = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(all_seen, "replica did not catch up to all 5 keys in time");

    // Verify every replicated value byte-for-byte.
    for (k, v) in pairs {
        send_resp(&mut reader, &[b"GET", k]);
        let header = read_line(&mut reader);
        let expected_header = format!("${}\r\n", v.len());
        assert_eq!(
            header, expected_header.as_bytes(),
            "key {:?}: header mismatch", String::from_utf8_lossy(k),
        );
        let payload = read_line(&mut reader);
        let mut expected_payload = v.to_vec();
        expected_payload.extend_from_slice(b"\r\n");
        assert_eq!(
            payload, expected_payload,
            "key {:?}: payload mismatch", String::from_utf8_lossy(k),
        );
    }

    drop(reader);
    drop(writer);
    // Shut down primary FIRST so the replica runner's blocking
    // `next_event` read sees peer EOF and unblocks; the runner then
    // sleeps the reconnect backoff (which checks `stop` afterwards),
    // and `replica.shutdown` completes within one backoff window.
    primary.shutdown();
    replica.shutdown();
}

/// T1.29.5 / T1.30 dynamic REPLICAOF e2e — a server brought up as
/// standalone (no `[replication]` config) takes a runtime `REPLICAOF
/// host port` command, starts mirroring an upstream primary's keyspace,
/// then takes `REPLICAOF NO ONE` and demotes back to standalone.
///
/// To install the per-shard senders that `cmd_replicaof` reaches into
/// (`crate::replica_state`), the test calls the public hook
/// `kevy::install_replica_senders_for_test` (exposed only under
/// `#[cfg(test)]` so the production surface stays minimal).
#[test]
fn replicaof_command_dynamically_attaches_to_primary() {
    // Primary on its own Runtime — same setup as the original e2e.
    let primary = Server::start(1);
    let mut writer = std::net::TcpStream::connect(("127.0.0.1", primary.port)).unwrap();
    let pairs: &[(&[u8], &[u8])] = &[
        (b"dy-alpha", b"A"),
        (b"dy-beta",  b"B"),
        (b"dy-gamma", b"C"),
    ];
    for (k, v) in pairs {
        send_resp(&mut writer, &[b"SET", k, v]);
        assert_eq!(read_line(&mut writer), b"+OK\r\n");
    }

    // Replica Runtime — has an inbox installed at startup (via
    // `with_replica_inboxes`) but NO initial runner. The sender for
    // that inbox is installed in the process-global slot so the
    // `cmd_replicaof` handler can find it.
    let (sender, receiver) = kevy_rt::replica_inbox_pair();
    kevy::install_replica_senders_for_test(vec![sender]);

    let replica_port = free_port_block(1) + 1;
    let replica_dir = tempdir::TempDir::new("kevy-dynamic-replica");
    let replica_dir_path = replica_dir.path().to_path_buf();
    // SAFETY: see Server::start.
    unsafe { std::env::set_var("KEVY_IO_URING", "0"); }
    let replica_stop = Arc::new(AtomicBool::new(false));
    let replica_stop_thread = replica_stop.clone();
    let replica_handle = std::thread::spawn(move || {
        let rt = kevy_rt::Runtime::new(
            [127, 0, 0, 1],
            replica_port,
            1,
            kevy::KevyCommands,
        )
        .with_data_dir(replica_dir_path)
        .with_aof(false)
        .with_replica_inboxes(vec![receiver]);
        let _ = rt.run(replica_stop_thread);
    });
    for _ in 0..400 {
        if std::net::TcpStream::connect(("127.0.0.1", replica_port)).is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    // Pre-REPLICAOF: ROLE on the replica reports master (no live
    // upstream). Verify before issuing the command.
    let mut admin = std::net::TcpStream::connect(("127.0.0.1", replica_port)).unwrap();
    send_resp(&mut admin, &[b"ROLE"]);
    let role_pre = {
        let _ = admin.set_read_timeout(Some(std::time::Duration::from_secs(2)));
        let mut buf = vec![0u8; 256];
        let n = admin.read(&mut buf).unwrap();
        buf[..n].to_vec()
    };
    assert!(
        role_pre.starts_with(b"*3\r\n$6\r\nmaster\r\n"),
        "expected master before REPLICAOF; got {:?}",
        String::from_utf8_lossy(&role_pre),
    );

    // REPLICAOF 127.0.0.1 <primary.replication_base>
    let primary_port_str = primary.replication_base.to_string();
    send_resp(&mut admin, &[b"REPLICAOF", b"127.0.0.1", primary_port_str.as_bytes()]);
    let reply = read_line(&mut admin);
    assert_eq!(reply, b"+OK\r\n", "REPLICAOF reply: {:?}", String::from_utf8_lossy(&reply));

    // Poll the replica until every key shows up — runner connects,
    // primary streams from offset 0, frames apply through the inbox
    // path.
    let mut reader = std::net::TcpStream::connect(("127.0.0.1", replica_port)).unwrap();
    let mut all_seen = false;
    for _ in 0..200 {
        let mut got_all = true;
        for (k, _v) in pairs {
            send_resp(&mut reader, &[b"GET", k]);
            let line = read_line(&mut reader);
            if line.starts_with(b"$-1") {
                got_all = false;
                break;
            }
            if line.starts_with(b"$") {
                let _ = read_line(&mut reader);
            }
        }
        if got_all {
            all_seen = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(all_seen, "replica didn't catch up after dynamic REPLICAOF");

    // ROLE should now report slave with the live upstream.
    send_resp(&mut admin, &[b"ROLE"]);
    let role_during = {
        let _ = admin.set_read_timeout(Some(std::time::Duration::from_secs(2)));
        let mut buf = vec![0u8; 256];
        let n = admin.read(&mut buf).unwrap();
        buf[..n].to_vec()
    };
    assert!(
        role_during.starts_with(b"*5\r\n$5\r\nslave\r\n"),
        "expected slave after REPLICAOF; got {:?}",
        String::from_utf8_lossy(&role_during),
    );

    // REPLICAOF NO ONE — demote.
    send_resp(&mut admin, &[b"REPLICAOF", b"NO", b"ONE"]);
    let reply = read_line(&mut admin);
    assert_eq!(reply, b"+OK\r\n");

    // ROLE should report master again.
    send_resp(&mut admin, &[b"ROLE"]);
    let role_after = {
        let _ = admin.set_read_timeout(Some(std::time::Duration::from_secs(2)));
        let mut buf = vec![0u8; 256];
        let n = admin.read(&mut buf).unwrap();
        buf[..n].to_vec()
    };
    assert!(
        role_after.starts_with(b"*3\r\n$6\r\nmaster\r\n"),
        "expected master after REPLICAOF NO ONE; got {:?}",
        String::from_utf8_lossy(&role_after),
    );

    drop(reader);
    drop(admin);
    drop(writer);
    primary.shutdown();
    // Replica side: the runner (if NO ONE didn't already stop it) is
    // also blocked-on-socket; stop_runners flushes it. Process-global
    // state cleared via NO ONE above already.
    kevy::install_replica_senders_for_test(Vec::new());
    replica_stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = replica_handle.join();
    drop(replica_dir);
}
