//! Replication integration tests over real sockets: the listener
//! accepts a replica handshake and replies `+ACK <gen> <offset>`, streams
//! live frames with acked-offset tracking, ships snapshots, and
//! honors dynamic `REPLICAOF` — plus the WAIT / REPL.* barrier verbs.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// In-process dispatcher: one KevyCommands per test thread, so
/// per-state caches (e.g. the SCRIPT cache) persist across calls
/// within a test.
fn dispatch<A: kevy_rt::ArgvView + ?Sized>(store: &mut kevy_store::Store, args: &A) -> Vec<u8> {
    thread_local! {
        static KEVY: kevy::KevyCommands = kevy::KevyCommands::new();
    }
    KEVY.with(|k| k.dispatch(store, args))
}

static START_GATE: Mutex<()> = Mutex::new(());

/// Hand out a process-unique block of `width + 1` consecutive ports.
/// An atomic bump makes blocks disjoint across the (now lock-free
/// parallel) tests in this binary — the probe-bind only guards
/// against ports held by OTHER processes; the old probe-then-drop
/// scheme had a TOCTOU window that parallel tests could race into.
/// A block of consecutive free ports, in a band this PROCESS owns.
///
/// `cargo test` compiles each test file into its own binary and runs them
/// concurrently, and every one of them used to start this counter at 21000.
/// A bind-check does not save you from that: kevy binds with SO_REUSEPORT, so
/// two servers in two test processes can hold the SAME port at once and the
/// kernel routes each connection to one of them by hash. That is how a
/// replication test that asserts "a quiet stream carries only pings" would
/// occasionally read a data frame — it had connected to a DIFFERENT test's
/// server, one that was busy being written to.
///
/// Seeding the band from the pid gives each test binary its own 512-port
/// window, so two of them cannot hand out the same port at all. The bind-check
/// stays as a second line against anything else on the box.
fn free_port_block(width: usize) -> u16 {
    use std::sync::atomic::{AtomicU16, Ordering};
    use std::sync::LazyLock;

    const BAND: u16 = 512;
    const LO: u16 = 21_000;
    const BANDS: u16 = 80; // 21_000 .. 61_960

    /// The first port of THIS process's band.
    static BAND_LO: LazyLock<u16> = LazyLock::new(|| {
        // Mix the pid so consecutive pids do not land in adjacent bands and
        // pids differing by a multiple of BANDS do not collide.
        let mixed = std::process::id().wrapping_mul(2_654_435_761) >> 16;
        LO + (mixed % u32::from(BANDS)) as u16 * BAND
    });
    static NEXT: LazyLock<AtomicU16> = LazyLock::new(|| AtomicU16::new(*BAND_LO));

    'retry: loop {
        let span = width as u16 + 1;
        let base = NEXT.fetch_add(span, Ordering::Relaxed);
        // Wrap inside our own band, never into someone else's.
        if base.checked_add(span).is_none() || base + span >= *BAND_LO + BAND {
            NEXT.store(*BAND_LO, Ordering::Relaxed);
            continue;
        }
        for i in 0..=width as u16 {
            if std::net::TcpListener::bind(("127.0.0.1", base + i)).is_err() {
                continue 'retry;
            }
        }
        return base;
    }
}

/// Poll `port` until something accepts, or fail by name.
///
/// The four copies of this loop that lived here waited 2 s and then fell
/// through in silence, which is how a slow start on a loaded runner turned
/// into a confusing failure much later — `spop_storm` spent a full minute
/// waiting for a replica that had never bound, then reported "replica never
/// caught up". Wait long enough that load alone cannot fail it, and say
/// which port did not come up when it genuinely does not.
fn wait_port(port: u16, what: &str) {
    // 60s. The first cut of this helper used 20s and made things WORSE
    // than the loops it replaced: `ReplicaServer::start` waited 2s and a
    // later connect loop waited another 60s, so unifying them at 20s cut
    // the total patience from 62s to 20s. covgate found that immediately —
    // it runs under llvm-cov instrumentation, where a runtime takes an
    // order of magnitude longer to bind than in a normal build.
    //
    // Waiting longer costs nothing on a healthy run (this returns the
    // moment the port answers) and only spends time on a run that is
    // already failing.
    let budget = patience();
    let deadline = std::time::Instant::now() + budget;
    while std::time::Instant::now() < deadline {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("{what} never bound port {port} within {budget:?}");
}

/// How long a test waits for something that is slow rather than broken.
///
/// 60s in a normal build, scaled by `KEVY_TEST_PATIENCE`. covgate sets
/// it because llvm-cov instrumentation slows boot by an order of
/// magnitude, and these budgets had been climbing one incident at a
/// time -- the replica accept loop went 30s -> 60s and still lost a run.
/// Scaling by environment beats another blind raise: a real hang in a
/// normal build still fails at 60s instead of inheriting the slow path's
/// patience, so the budget tracks machine speed rather than the worst
/// case ever seen.
fn patience() -> std::time::Duration {
    let mult: f64 = std::env::var("KEVY_TEST_PATIENCE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1.0);
    std::time::Duration::from_secs_f64(60.0 * mult)
}

/// Connect to `port`, retrying until `patience()` runs out.
///
/// A bound port is not a served port: the runtime binds (which
/// `wait_port` sees) a moment before its accept loop serves, and under
/// instrumentation that moment is long.
/// Connect if it can, within a short budget, and say so if it cannot.
///
/// Diagnostics use this rather than a panicking retry: a diagnostic that
/// panics REPLACES the failure it was there to explain. This test used to
/// report "replica (diagnostic) never became ready" whenever the replica
/// fell behind — naming the wrong thing entirely, and sending two rounds
/// of budget-raising after a startup problem that was never the problem.
///
/// The replica's own [`ReplicaServer::connect_or_explain`] is the retrying
/// form now, and it reports thread liveness on timeout.
fn try_connect(port: u16) -> Option<std::net::TcpStream> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if let Ok(s) = std::net::TcpStream::connect(("127.0.0.1", port)) {
            return Some(s);
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    None
}

struct Server {
    #[allow(dead_code)]
    port: u16,
    replication_base: u16,
    nshards: usize,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    /// `Some` for the normal one-shot lifecycle; taken out by
    /// [`Server::stop_take_dir`] when a test restarts a server on
    /// the same data dir (feed-generation continuity tests).
    _dir: Option<TmpDir>,
}

// Temp dirs come from `kevy-tmpdir`, not a local stand-in. The one that
// lived here derived its "unique" path from `SystemTime::now().as_nanos()`
// alone — no pid, no counter. Two tests starting in the same nanosecond
// (the harness runs them in parallel, and a loaded machine bunches them)
// got the SAME directory, so the second server booted on the first's data:
// it found a feed-generation high-water with no clean-shutdown marker,
// read that as an unclean boot, and bumped the generation. That is the
// `+ACK 2` where the test asserted `+ACK 1`, and why it only ever failed
// under load. `kevy-tmpdir` exists because this exact class of hand-rolled
// scheme was wrong in nine files; this one was missed.
use kevy_tmpdir::TmpDir;

impl Server {
    fn start(nshards: usize) -> Server {
        Self::start_in(nshards, TmpDir::new("kevy-replication-test"))
    }

    /// [`Self::start`] on a caller-supplied data dir — the restart
    /// half of the feed-generation tests (same dir, new process
    /// lifecycle).
    fn start_in(nshards: usize, dir: TmpDir) -> Server {
        let _gate = START_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        // compat port + n replication ports (no cluster mode here).
        let base = free_port_block(nshards);
        let port = base;
        let replication_base = base + 1;
        let dir_path = dir.path().to_path_buf();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();

        // Force the epoll/kqueue reactor — the replication listener is
        // gated off on io_uring (see the `Runtime::run` startup check).
        // SAFETY: integration test owns its own process state; setting
        // an env var here is safe since no other thread reads
        // KEVY_IO_URING in parallel.
        unsafe {
            std::env::set_var("KEVY_IO_URING", "0");
        }

        let handle = std::thread::spawn(move || {
            let rt = kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(nshards)).bind([127, 0, 0, 1], port).shards(nshards)
            .with_data_dir(dir_path)
            .with_aof(false)
            .with_replication(true, 1024 * 1024)
            // Scales with `patience()` for the same reason the waits do.
            // The default is 60s, the same order as an instrumented run of
            // this suite -- so under covgate the replica's slot expired
            // while the test was still waiting for it, and no amount of
            // extra waiting could help: the thing being waited for had
            // lost its backlog position.
            .with_replication_reconnect_window(patience().as_millis() as u32)
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
        // One budget for every port wait in this file. This loop already
        // carried its own, widened once from 2s to 10s for a loaded macOS
        // runner — and a Linux runner then blew the 10s too, booting
        // nshards+1 runtimes while the rest of the suite ran in parallel.
        // Chasing it a third time with another hand-picked number is how a
        // budget ends up wrong per-file; `wait_port` is the one place to
        // set it.
        for p in ports {
            wait_port(p, "runtime");
        }
        Server {
            port,
            replication_base,
            nshards,
            stop,
            handle: Some(handle),
            _dir: Some(dir),
        }
    }

    fn shutdown(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }

    /// Stop the runtime but KEEP the data dir alive (hand it back to
    /// the caller) so a second server can boot on it — the restart
    /// half of the feed-generation tests.
    fn stop_take_dir(mut self) -> TmpDir {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        self._dir.take().expect("dir still owned")
    }
}

fn replicate_from(offset: &str, id: &str) -> Vec<u8> {
    // Raw 6-arg 4.0 handshake with a gen-0 (fresh / no-claim)
    // generation — the shape every one of these streaming tests
    // wants: a fresh server's feed gen is 1, and the pump's fence
    // adopts it for a gen-0 offset-0 claim.
    let mut v = Vec::new();
    v.extend_from_slice(b"*6\r\n");
    for arg in [
        b"REPLICATE".as_slice(),
        b"FROM",
        b"0",
        offset.as_bytes(),
        b"ID",
        id.as_bytes(),
    ] {
        v.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
        v.extend_from_slice(arg);
        v.extend_from_slice(b"\r\n");
    }
    v
}

/// A quiet streaming conn carries 1 Hz `+PING <n>` heartbeats,
/// so the reply shape is "the ACK and then pings" —
/// assert the ACK prefix and that everything after is ping lines.
fn assert_ack_then_pings(reply: &[u8], want_ack: &[u8]) {
    assert!(
        reply.starts_with(want_ack),
        "reply must start with {:?}, got {:?}",
        String::from_utf8_lossy(want_ack),
        String::from_utf8_lossy(reply),
    );
    for line in reply[want_ack.len()..].split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        assert!(
            line.is_empty() || line.starts_with(b"+PING "),
            "unexpected non-ping bytes after ACK: {:?}",
            String::from_utf8_lossy(line),
        );
    }
}

fn read_to_eof(s: &mut std::net::TcpStream) -> Vec<u8> {
    // The 1 Hz heartbeats keep feeding the per-read timeout, so a
    // quiet-socket read loop never starves — bound by WALL CLOCK too.
    let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(2)));
    let start = std::time::Instant::now();
    let mut out = Vec::new();
    let mut chunk = [0u8; 256];
    while start.elapsed() < std::time::Duration::from_secs(3) {
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
    // After `+ACK` the conn transitions to Streaming.
    // With no source mutations, the replica
    // just sees the +ACK and a quiet socket — `read_to_eof` returns
    // when its 2 s timeout elapses, NOT when the server closes.
    let server = Server::start(1);
    let mut s = std::net::TcpStream::connect(("127.0.0.1", server.replication_base)).unwrap();
    s.write_all(&replicate_from("0", "replica-a")).unwrap();
    let reply = read_to_eof(&mut s);
    assert_ack_then_pings(&reply, b"+ACK 1 0\r\n");
    server.shutdown();
}

#[test]
fn handshake_with_nonzero_offset_echoed_in_ack_then_fence_ships() {
    // The ACK still ECHOES the requested offset (handshake layer is
    // fence-agnostic), but a gen-0 claim with a NONZERO offset is an
    // unsafe resume — the pump's generation fence must answer with a
    // snapshot ship, never a quiet streaming socket (T8).
    let server = Server::start(1);
    let mut s = std::net::TcpStream::connect(("127.0.0.1", server.replication_base)).unwrap();
    s.write_all(&replicate_from("12345", "node-7")).unwrap();
    let reply = read_to_eof(&mut s);
    assert!(
        reply.starts_with(b"+ACK 1 12345\r\n"),
        "ACK echoes the requested offset, got {:?}",
        String::from_utf8_lossy(&reply),
    );
    let rest = &reply[b"+ACK 1 12345\r\n".len()..];
    assert!(
        rest.windows(b"+SNAPSHOT\r\n".len()).any(|w| w == b"+SNAPSHOT\r\n"),
        "generation fence must ship a snapshot for an unverifiable resume claim, got {:?}",
        String::from_utf8_lossy(rest),
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
    let dir = TmpDir::new("kevy-replication-disabled");
    let dir_path = dir.path().to_path_buf();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    // SAFETY: see other set_var call in this file — single test thread.
    unsafe {
        std::env::set_var("KEVY_IO_URING", "0");
    }
    let handle = std::thread::spawn(move || {
        let rt = kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(1)).bind([127, 0, 0, 1], base).shards(1)
            .with_data_dir(dir_path)
            .with_aof(false);
        // No .with_replication / .with_replication_listener calls.
        let _ = rt.run(stop_thread);
    });
    // Wait for compat port.
    wait_port(base, "server");
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
    let ack = read_at_least(&mut replica, b"+ACK 1 0\r\n".len());
    assert!(ack.starts_with(b"+ACK 1 0\r\n"), "got {:?}", String::from_utf8_lossy(&ack));

    // Now a regular client on the main port issues a SET.
    let mut client = std::net::TcpStream::connect(("127.0.0.1", server.port)).unwrap();
    send_resp(&mut client, &[b"SET", b"foo", b"bar"]);
    let ok = read_line(&mut client);
    assert_eq!(ok, b"+OK\r\n");

    // Replica should receive the frame. The +ACK may or may not have
    // been fully consumed by `read_at_least`; pull the leftover bytes
    // (everything after the ACK we already saw).
    let mut buf = ack[b"+ACK 1 0\r\n".len()..].to_vec();
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
    // Strip any out-of-band +PING heartbeat lines before the frame.
    let mut start = 0usize;
    while buf.len() > start && buf[start] == b'+' {
        match buf[start..].windows(2).position(|w| w == b"\r\n") {
            Some(p) => start += p + 2,
            None => break,
        }
    }
    let buf = &buf[start..];
    let (offset, argv, used) =
        kevy_replicate::wire::decode_frame(buf).expect("decode frame");
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
    let ack = read_at_least(&mut replica, b"+ACK 1 0\r\n".len());
    assert!(ack.starts_with(b"+ACK 1 0\r\n"));

    let mut client = std::net::TcpStream::connect(("127.0.0.1", server.port)).unwrap();
    for i in 0..5 {
        send_resp(&mut client, &[b"SET", format!("k{i}").as_bytes(), format!("v{i}").as_bytes()]);
        let ok = read_line(&mut client);
        assert_eq!(ok, b"+OK\r\n");
    }

    // Collect bytes after the ACK until we have 5 decoded frames.
    let mut buf = ack[b"+ACK 1 0\r\n".len()..].to_vec();
    let mut frames: Vec<(u64, kevy_resp::Argv)> = Vec::new();
    let mut cursor = 0usize;
    while frames.len() < 5 {
        if buf.len() - cursor > 0 {
            // Skip out-of-band +PING heartbeat lines between frames.
            if buf[cursor] == b'+'
                && let Some(p) = buf[cursor..].windows(2).position(|w| w == b"\r\n")
            {
                cursor += p + 2;
                continue;
            }
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
            let ack = read_at_least(&mut r, b"+ACK 1 0\r\n".len());
            assert!(ack.starts_with(b"+ACK 1 0\r\n"));
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
        let mut buf = ack[b"+ACK 1 0\r\n".len()..].to_vec();
        let mut cursor = 0usize;
        let _ = r.set_read_timeout(Some(std::time::Duration::from_millis(500)));
        loop {
            // Skip out-of-band +PING heartbeat lines between frames.
            if buf.len() > cursor
                && buf[cursor] == b'+'
                && let Some(p) = buf[cursor..].windows(2).position(|w| w == b"\r\n")
            {
                cursor += p + 2;
                continue;
            }
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
    let dir = TmpDir::new("kevy-snapshot-ship");
    let dir_path = dir.path().to_path_buf();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    // SAFETY: see Server::start.
    unsafe {
        std::env::set_var("KEVY_IO_URING", "0");
    }
    let handle = std::thread::spawn(move || {
        let rt = kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(1)).bind([127, 0, 0, 1], port).shards(1)
            .with_data_dir(dir_path)
            .with_aof(false)
            .with_replication(true, buffer_size)
            .with_replication_listener(replication_base);
        let _ = rt.run(stop_thread);
    });
    for p in [port, replication_base] {
        wait_port(p, "runtime");
    }
    Server {
        port,
        replication_base,
        nshards: 1,
        stop,
        handle: Some(handle),
        _dir: Some(dir),
    }
}

#[test]
fn snapshot_ship_triggers_when_replica_falls_behind_backlog() {
    use kevy_replicate::replica::{ReplicaClient, ReplicaEvent};

    // A replica that asks for `from_offset = 0` after the
    // primary's backlog has evicted offset 0 triggers a snapshot
    // ship. Verify the full sequence: SnapshotBegin → ≥ 1 Chunk →
    // SnapshotEnd { ack_offset, routed: false } → expected_offset advances to
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

    // First NON-PING event must be SnapshotBegin (TooOld → ship);
    // heartbeats may interleave before the ship starts.
    loop {
        match client.next_event().expect("event").expect("ok") {
            ReplicaEvent::SnapshotBegin => break,
            kevy_replicate::replica::ReplicaEvent::Ping { .. } => continue,
            other => panic!("expected SnapshotBegin, got {other:?}"),
        }
    }

    // Accumulate chunks until SnapshotEnd.
    let mut snapshot_bytes = Vec::new();
    let ack_offset = loop {
        match client.next_event().expect("event").expect("ok") {
            ReplicaEvent::SnapshotChunk(bytes) => snapshot_bytes.extend(bytes),
            ReplicaEvent::SnapshotEnd { ack_offset } => break ack_offset,
            kevy_replicate::replica::ReplicaEvent::Ping { .. } => continue,
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
    // Just check the prefix — the full load_snapshot round-trip is
    // covered by `snapshot_ship_loaded_into_local_store_matches_primary`.
    assert!(snapshot_bytes.len() > 8, "snapshot too small");
    assert_eq!(&snapshot_bytes[..8], b"KEVYSNAP", "snapshot magic");

    drop(client);
    server.shutdown();
}

#[test]
fn snapshot_ship_loaded_into_local_store_matches_primary() {
    use kevy_replicate::replica::{ReplicaClient, ReplicaEvent};

    // Full primary→replica round-trip via snapshot ship.
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
            kevy_replicate::replica::ReplicaEvent::Ping { .. } => continue,
            other => panic!("unexpected event: {other:?}"),
        }
    };
    assert_eq!(ack_offset, pairs.len() as u64);

    // Load the streamed snapshot into a fresh local Store via the
    // `load_snapshot_from` API. The Store is the primitive
    // single-shard kevy_store::Store; for multi-shard replicas the
    // caller routes by hash before load. Single-shard is enough here
    // to prove the contract.
    let mut local_store = kevy_store::Store::new();
    kevy_persist::load_snapshot_from(&mut local_store, std::io::Cursor::new(&snapshot_bytes))
        .expect("load_snapshot_from");

    // GET each primary-written key against the loaded local store
    // and verify byte-equivalence. Uses kevy::dispatch (the same
    // in-process apply path the streaming tests use).
    for (k, v) in &pairs {
        let argv = kevy::Argv::from(vec![b"GET".to_vec(), k.as_bytes().to_vec()]);
        let reply = dispatch(&mut local_store, &argv);
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

    // E2e: a fresh replica joins a primary whose
    // backlog has already evicted offset 0 → it takes the snapshot
    // path; after `SnapshotEnd { ack_offset, routed: false }` the replica receives
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
            kevy_replicate::replica::ReplicaEvent::Ping { .. } => continue,
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
                let _ = dispatch(&mut local_store, &frame.argv);
            }
            kevy_replicate::replica::ReplicaEvent::Ping { .. } => continue,
            other => panic!("live frame {i}: expected Frame, got {other:?}"),
        }
    }

    // Stage 6: every key — snapshot-loaded and live-frame-applied —
    // GETs byte-equivalent on the local store. That's the contract.
    for (k, v) in pre.iter().chain(post.iter()) {
        let argv = kevy::Argv::from(vec![b"GET".to_vec(), k.as_bytes().to_vec()]);
        let reply = dispatch(&mut local_store, &argv);
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
    // Prove the apply path. After streaming N writes from
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
        let _reply = dispatch(&mut local_store, &frame.argv);
    }

    // For every key written to primary, GET on the local replica
    // store returns byte-equivalent value. This is the contract:
    // applied(primary) == applied(replica).
    for (k, v) in pairs {
        let argv = kevy::Argv::from(vec![b"GET".to_vec(), k.to_vec()]);
        let reply = dispatch(&mut local_store, &argv);
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
    // `ROLE` on a primary returns `["master", <offset>, []]`
    // where <offset> tracks the shard's replication source. After
    // N writes the offset published per tick (~100 ms) should reflect
    // the writes — verify via the wire-level ROLE reply.

    let server = Server::start(1);
    let mut s = std::net::TcpStream::connect(("127.0.0.1", server.port)).unwrap();

    // Before any writes — first poll within tick interval. Wait for
    // the publish loop to fire at least once + observe ROLE reply.
    let mut last = Vec::new();
    // 5s, not 0.4s: this waits on the per-tick publish loop (100ms
    // default), so the old budget was four ticks.
    for _ in 0..500 {
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
    // 10s, not 1s: same per-tick publish path as above.
    for _ in 0..1000 {
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
        assert_ack_then_pings(&reply, b"+ACK 1 0\r\n");
    }
    server.shutdown();
}

/// End-to-end: a SECOND kevy_rt::Runtime spun up as
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
    _dir: TmpDir,
}

impl ReplicaServer {
    fn start(upstream_replication_port: u16) -> Self {
        let _gate = START_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let port = free_port_block(0) + 1; // one free port (no replication listener / cluster)
        let dir = TmpDir::new("kevy-replica-rt");
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
            let rt = kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(1)).bind([127, 0, 0, 1], port).shards(1)
            .with_data_dir(dir_path)
            .with_aof(false)
            .with_replica_inboxes(vec![receiver]);
            let _ = rt.run(stop_runtime_thread);
        });
        wait_port(port, "server");

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
                        kevy_replicate::replica::ReplicaEvent::Ping { .. } => continue,
                                kevy_replicate::replica::ReplicaEvent::SnapshotBegin => {
                                    kevy_rt::ReplicaApply::SnapshotBegin
                                }
                                kevy_replicate::replica::ReplicaEvent::SnapshotChunk(b) => {
                                    kevy_rt::ReplicaApply::SnapshotChunk(b)
                                }
                                kevy_replicate::replica::ReplicaEvent::SnapshotEnd { ack_offset } => {
                                    from_offset = ack_offset;
                                    kevy_rt::ReplicaApply::SnapshotEnd { ack_offset, routed: false, gate: None }
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

    /// Whether the replica's in-process runtime thread is still running.
    /// `false` means it panicked and exited — which is a different failure
    /// from "slow to accept", and the one a bare connect-timeout cannot
    /// distinguish. Used to make that timeout say which it was.
    fn runtime_alive(&self) -> bool {
        self.rt_handle.as_ref().is_some_and(|h| !h.is_finished())
    }

    /// [`connect_retry`] to this replica, but if the budget runs out, say
    /// whether the runtime thread is still alive. A bare connect-timeout
    /// reads the same whether the replica is merely slow to accept or has
    /// panicked and gone — and on a loaded CI runner those want opposite
    /// responses (widen patience vs. find the crash). Checking the thread
    /// handle separates them.
    fn connect_or_explain(&self, what: &str) -> std::net::TcpStream {
        let budget = patience();
        let deadline = std::time::Instant::now() + budget;
        while std::time::Instant::now() < deadline {
            if let Some(s) = try_connect(self.port) {
                return s;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let state = if self.runtime_alive() {
            "the runtime thread is still alive — it bound the port but never \
             served in time: a slow runner, widen KEVY_TEST_PATIENCE"
        } else {
            "the runtime thread has EXITED — the replica panicked rather than \
             fell behind; look for its panic, not a timing budget"
        };
        panic!("{what} never became ready on port {} within {budget:?}. {state}", self.port);
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
    // Retry connect — on a heavily-loaded CI runner the runtime may
    // bind the port (which start's poll saw) but the accept loop
    // needs an extra moment before serving on it. llvm-cov
    // instrumentation (covgate) slows boot severely — 20ms × 3000
    // = 60s hard cap (30s was observed insufficient once the suite
    // grew: parallel test threads + instrumented boot).
    let mut reader = replica.connect_or_explain("replica accept loop");
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
                // bulk header — consume the payload line + crlf BEFORE
                // deciding anything: an empty bulk ($0) still carries its
                // terminating CRLF, and breaking with it unread desyncs
                // every later response by one — the verify loop then reads
                // a stale header for the wrong key (the $4-for-alpha CI
                // failure, 2026-08-02).
                let _ = read_line(&mut reader);
                if line.starts_with(b"$0\r") {
                    // An EMPTY value is not "caught up" — and it should be
                    // impossible (the inbox applies on the serving shard
                    // thread). Log loudly so a recurrence carries evidence.
                    eprintln!(
                        "replica returned $0 (empty bulk) for {:?} mid-catch-up",
                        String::from_utf8_lossy(k)
                    );
                    got_all = false;
                    break;
                }
            } else {
                // -ERR / unexpected: single-line reply, not caught up.
                got_all = false;
                break;
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

/// Read one RESP2 reply off `s`, returning any bulk payloads it
/// carried: an array of bulks (SMEMBERS / count-form SPOP) yields each
/// member, a bare bulk yields one, a null bulk / `+OK` / `:n` yields
/// none. Doubles as a discard-the-reply consumer for the pop storm.
fn read_resp_bulks(s: &mut std::net::TcpStream) -> Vec<Vec<u8>> {
    let head = read_line(s);
    match head[0] {
        b'+' | b':' => Vec::new(),
        b'$' => {
            let n: i64 = std::str::from_utf8(&head[1..head.len() - 2])
                .unwrap()
                .parse()
                .unwrap();
            if n < 0 {
                return Vec::new();
            }
            let mut payload = vec![0u8; n as usize + 2];
            s.read_exact(&mut payload).unwrap();
            payload.truncate(n as usize);
            vec![payload]
        }
        b'*' => {
            let n: i64 = std::str::from_utf8(&head[1..head.len() - 2])
                .unwrap()
                .parse()
                .unwrap();
            let mut out = Vec::new();
            for _ in 0..n.max(0) {
                out.extend(read_resp_bulks(s));
            }
            out
        }
        other => panic!(
            "unexpected RESP tag {other:?} in {:?}",
            String::from_utf8_lossy(&head)
        ),
    }
}

/// Sorted SMEMBERS of `key` over the wire.
fn smembers_sorted(s: &mut std::net::TcpStream, key: &[u8]) -> Vec<Vec<u8>> {
    send_resp(s, &[b"SMEMBERS", key]);
    let mut m = read_resp_bulks(s);
    m.sort();
    m
}

/// v4 SPOP is genuinely random — so the replication stream must carry
/// the EFFECT (`SREM key <popped…>`), never the verb: a replica
/// re-running `SPOP key n` draws its own random members and silently
/// diverges (the repligate failure shape: replica churning to a
/// different digest while the primary is stopped). Storm-pop four
/// 50-member sets on the primary — bare and count forms interleaved,
/// one set drained to empty for the Suppress path — then compare every
/// set member-for-member across primary and replica. Red under verb
/// propagation.
#[test]
fn spop_storm_keeps_replica_sets_identical() {
    let primary = Server::start(1);
    let mut writer = std::net::TcpStream::connect(("127.0.0.1", primary.port)).unwrap();

    let keys: Vec<Vec<u8>> = (0..4).map(|k| format!("spop-set-{k}").into_bytes()).collect();
    let all: Vec<Vec<u8>> = (0..50).map(|i| format!("m{i:02}").into_bytes()).collect();
    for key in &keys {
        let mut argv: Vec<&[u8]> = vec![b"SADD", key];
        argv.extend(all.iter().map(Vec::as_slice));
        send_resp(&mut writer, &argv);
        assert_eq!(read_line(&mut writer), b":50\r\n");
    }

    // Replica attaches from offset 0 — it receives the SADDs above and
    // the whole storm below as live frames.
    let replica = ReplicaServer::start(primary.replication_base);

    // The storm: 10 rounds × (1 bare pop + 1 two-member pop) per key.
    for _ in 0..10 {
        for key in &keys {
            send_resp(&mut writer, &[b"SPOP", key]);
            let _ = read_resp_bulks(&mut writer);
            send_resp(&mut writer, &[b"SPOP", key, b"2"]);
            let _ = read_resp_bulks(&mut writer);
        }
    }
    // Drain the last set completely, then pop it again while empty —
    // the empty pop must stream NOTHING (Suppress), not a no-op verb.
    send_resp(&mut writer, &[b"SPOP", &keys[3], b"100"]);
    let drained = read_resp_bulks(&mut writer);
    assert_eq!(drained.len(), 20, "set 3 should have had 20 members left");
    send_resp(&mut writer, &[b"SPOP", &keys[3]]);
    assert_eq!(read_line(&mut writer), b"$-1\r\n");

    // Fence: frames apply in order on the single shard, so once this
    // SET is visible on the replica every prior SREM has landed too.
    send_resp(&mut writer, &[b"SET", b"spop-fence", b"done"]);
    assert_eq!(read_line(&mut writer), b"+OK\r\n");

    // Connect to the replica (retry — see server_as_replica test) and
    // poll the fence key.
    let mut reader = replica.connect_or_explain("replica accept loop");
    let mut fenced = false;
    // 60s, not 10s: the replica attaches mid-storm and has to replay the
    // backlog, and on a loaded runner that is slow rather than broken. A
    // budget that load alone can exhaust reports a real bug that is not
    // there — this test did exactly that in CI while passing locally in
    // 0.13s.
    let budget = patience();
    let deadline = std::time::Instant::now() + budget;
    while std::time::Instant::now() < deadline {
        send_resp(&mut reader, &[b"GET", b"spop-fence"]);
        if read_resp_bulks(&mut reader) == vec![b"done".to_vec()] {
            fenced = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    if !fenced {
        // What the replica DID see — "never caught up" alone cannot tell
        // "the runner never attached" from "it attached and fell behind".
        //
        // On a FRESH connection: the polling loop above leaves `reader`
        // mid-reply whenever it gives up between a request and its
        // response, and reading DBSIZE off it then returns whatever was
        // still queued. A previous failure of this test reported
        // `DBSIZE = *1` -- an array header, which DBSIZE cannot return --
        // so the one line that was supposed to explain the failure was
        // itself desynced and said nothing.
        let detail = match try_connect(replica.port) {
            Some(mut fresh) => {
                send_resp(&mut fresh, &[b"DBSIZE"]);
                let dbsize = read_line(&mut fresh);
                send_resp(&mut fresh, &[b"GET", b"spop-fence"]);
                let fence = read_line(&mut fresh);
                format!(
                    "replica DBSIZE = {}\n\
                     replica GET spop-fence = {}",
                    String::from_utf8_lossy(&dbsize).trim_end(),
                    String::from_utf8_lossy(&fence).trim_end(),
                )
            }
            // Still a real answer about the replica, and a different one:
            // it is not merely behind, it is not accepting. Say whether its
            // runtime thread is still alive — "slow to accept under load" and
            // "the replica panicked and is gone" want opposite responses, and
            // a bare "not accepting" cannot tell them apart.
            None => format!(
                "replica is not accepting connections on port {}; runtime thread {}",
                replica.port,
                if replica.runtime_alive() { "still ALIVE (slow, widen KEVY_TEST_PATIENCE)" } else { "has EXITED (panicked, find the crash)" },
            ),
        };
        panic!(
            "replica never caught up to the post-storm fence within {budget:?}\n{detail}"
        );
    }

    // Member-for-member equality on every set. Under verb propagation
    // the replica drew its own 30 random members per set — the odds of
    // matching are astronomically against.
    for (i, key) in keys.iter().enumerate() {
        let on_primary = smembers_sorted(&mut writer, key);
        let on_replica = smembers_sorted(&mut reader, key);
        assert_eq!(
            on_primary.len(),
            if i == 3 { 0 } else { 20 },
            "primary set {i}: unexpected survivor count"
        );
        assert_eq!(
            on_replica, on_primary,
            "set {i} diverged between primary and replica — SPOP verb replayed?"
        );
    }

    drop(reader);
    drop(writer);
    // Primary first — see server_as_replica_applies_upstream_writes.
    primary.shutdown();
    replica.shutdown();
}

/// Dynamic REPLICAOF e2e — a server brought up as
/// standalone (no `[replication]` config) takes a runtime `REPLICAOF
/// host port` command, starts mirroring an upstream primary's keyspace,
/// then takes `REPLICAOF NO ONE` and demotes back to standalone.
///
/// The per-shard inbox pair lives in the replica's own
/// `RuntimeState`; the test wires the receivers into its hand-built
/// runtime via `take_replica_inboxes`, exactly like `kevy::serve`.
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

    // Replica Runtime — its KevyCommands state allocates the inbox
    // pair; taking the receivers here wires them into the runtime so
    // the `cmd_replicaof` handler can spawn runners against them.
    let replica_commands = kevy::KevyCommands::sharded(1);
    let receivers = replica_commands
        .state()
        .take_replica_inboxes()
        .expect("fresh state");

    let replica_port = free_port_block(1) + 1;
    let replica_dir = TmpDir::new("kevy-dynamic-replica");
    let replica_dir_path = replica_dir.path().to_path_buf();
    // SAFETY: see Server::start.
    unsafe { std::env::set_var("KEVY_IO_URING", "0"); }
    let replica_stop = Arc::new(AtomicBool::new(false));
    let replica_stop_thread = replica_stop.clone();
    let replica_handle = std::thread::spawn(move || {
        let rt = kevy_rt::Runtime::builder(replica_commands).bind([127, 0, 0, 1], replica_port).shards(1)
        .with_data_dir(replica_dir_path)
        .with_aof(false)
        .with_replica_inboxes(receivers);
        let _ = rt.run(replica_stop_thread);
    });
    wait_port(replica_port, "server");

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
    // Replica side: NO ONE above already stopped the runner fleet;
    // the runtime's own state drops with it.
    replica_stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = replica_handle.join();
    drop(replica_dir);
}

// ════════════════════════════════════════════════════════════════════
// WAIT / REPL.TOKEN / REPL.WAIT
// ════════════════════════════════════════════════════════════════════

/// A replica Runtime attached to `primary` through the REAL runner
/// fleet (`REPLICAOF` over the wire → `ReplicaRunner` → ACKs flow),
/// so the primary's slot table sees genuine acked offsets — what WAIT
/// counts and REPL.WAIT's gen registry learns from.
struct AttachedReplica {
    port: u16,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    _dir: TmpDir,
}

impl AttachedReplica {
    fn start(primary_replication_port: u16) -> Self {
        let commands = kevy::KevyCommands::sharded(1);
        let receivers = commands.state().take_replica_inboxes().expect("fresh state");
        let _gate = START_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let port = free_port_block(1) + 1;
        let dir = TmpDir::new("kevy-v316-replica");
        let dir_path = dir.path().to_path_buf();
        // SAFETY: see Server::start.
        unsafe { std::env::set_var("KEVY_IO_URING", "0"); }
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let handle = std::thread::spawn(move || {
            let rt = kevy_rt::Runtime::builder(commands).bind([127, 0, 0, 1], port).shards(1)
                .with_data_dir(dir_path)
                .with_aof(false)
                .with_replica_inboxes(receivers);
            let _ = rt.run(stop_thread);
        });
        wait_port(port, "server");
        drop(_gate);
        // REPLICAOF over the wire — spawns the real runner fleet.
        let mut admin = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        let port_str = primary_replication_port.to_string();
        send_resp(&mut admin, &[b"REPLICAOF", b"127.0.0.1", port_str.as_bytes()]);
        assert_eq!(read_line(&mut admin), b"+OK\r\n");
        Self { port, stop, handle: Some(handle), _dir: dir }
    }

    fn shutdown(mut self) {
        // REPLICAOF NO ONE over the wire — stops the runner fleet so
        // no runner blocks on its upstream socket across the join.
        if let Ok(mut admin) = std::net::TcpStream::connect(("127.0.0.1", self.port)) {
            send_resp(&mut admin, &[b"REPLICAOF", b"NO", b"ONE"]);
            let _ = read_line(&mut admin);
        }
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Read one `:N\r\n` integer reply.
fn read_int(s: &mut std::net::TcpStream) -> i64 {
    let line = read_line(s);
    assert!(line.starts_with(b":"), "expected integer, got {:?}", String::from_utf8_lossy(&line));
    std::str::from_utf8(&line[1..line.len() - 2]).unwrap().parse().unwrap()
}

/// Read a flat `*N` array of `:int` elements.
fn read_int_array(s: &mut std::net::TcpStream) -> Vec<i64> {
    let header = read_line(s);
    assert!(header.starts_with(b"*"), "expected array, got {:?}", String::from_utf8_lossy(&header));
    let n: usize = std::str::from_utf8(&header[1..header.len() - 2]).unwrap().parse().unwrap();
    (0..n).map(|_| read_int(s)).collect()
}

/// Poll the replica until it has learned the upstream generation off
/// the 1 Hz heartbeat (REPL.TOKEN on a replica reports the per-runner
/// view; gen 0 = not learned yet).
fn wait_replica_gen_learned(replica_port: u16) -> u64 {
    let mut c = std::net::TcpStream::connect(("127.0.0.1", replica_port)).unwrap();
    for _ in 0..200 {
        send_resp(&mut c, &[b"REPL.TOKEN"]);
        let pairs = read_int_array(&mut c);
        if pairs.len() == 2 && pairs[0] > 0 {
            return pairs[0] as u64;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    panic!("replica never learned the upstream generation from the heartbeat");
}

/// Spawn a REAL kevy primary process (the debug binary cargo builds
/// for this test crate) — the faithful topology for primary+replica
/// pairs (one server per process, each with its own state).
fn spawn_primary_process(replication_base: u16) -> (kevy_chaos::Harness, u16, std::path::PathBuf) {
    let port = kevy_chaos::pick_free_port().expect("primary port");
    let dir = std::env::temp_dir().join(format!("kevy-v316-primary-{port}"));
    let _ = std::fs::remove_dir_all(&dir);
    let cfg = kevy_chaos::HarnessConfig {
        kevy_bin: std::path::PathBuf::from(env!("CARGO_BIN_EXE_kevy")),
        threads: 1,
        ..kevy_chaos::HarnessConfig::new(dir.clone(), port)
            .with_fsync("everysec")
            .with_extra_toml(format!(
                "[replication]\nrole = \"primary\"\nlisten_port_base = {replication_base}\n"
            ))
    };
    let primary = kevy_chaos::Harness::spawn(cfg).expect("spawn primary kevy");
    (primary, port, dir)
}

#[test]
fn wait_with_no_replica_times_out_to_zero_and_wait_zero_is_immediate() {
    let server = Server::start(1);
    let mut c = std::net::TcpStream::connect(("127.0.0.1", server.port)).unwrap();
    send_resp(&mut c, &[b"SET", b"w0", b"v"]);
    assert_eq!(read_line(&mut c), b"+OK\r\n");
    // numreplicas 1, timeout 200 ms, zero replicas → parks, then :0.
    let t0 = std::time::Instant::now();
    send_resp(&mut c, &[b"WAIT", b"1", b"200"]);
    assert_eq!(read_int(&mut c), 0);
    let elapsed = t0.elapsed();
    assert!(
        elapsed >= std::time::Duration::from_millis(150),
        "WAIT 1 200 with no replica should park ~200ms, returned in {elapsed:?}"
    );
    // numreplicas 0 is satisfied by definition — immediate answer.
    let t0 = std::time::Instant::now();
    send_resp(&mut c, &[b"WAIT", b"0", b"5000"]);
    assert_eq!(read_int(&mut c), 0);
    assert!(t0.elapsed() < std::time::Duration::from_secs(1), "WAIT 0 must not park");
    server.shutdown();
}

#[test]
fn wait_one_with_live_replica_returns_at_least_one() {
    let replication_base = kevy_chaos::pick_free_port().expect("repl port");
    let (primary, pport, pdir) = spawn_primary_process(replication_base);
    let replica = AttachedReplica::start(replication_base);
    let mut c = std::net::TcpStream::connect(("127.0.0.1", pport)).unwrap();
    send_resp(&mut c, &[b"SET", b"w1", b"v"]);
    assert_eq!(read_line(&mut c), b"+OK\r\n");
    // The replica ACKs on the 100ms cadence + 1s heartbeat; a 5s
    // budget is comfortable on a loaded CI box.
    let _ = c.set_read_timeout(Some(std::time::Duration::from_secs(8)));
    send_resp(&mut c, &[b"WAIT", b"1", b"5000"]);
    let n = read_int(&mut c);
    assert!(n >= 1, "expected at least 1 acked replica, got {n}");
    replica.shutdown();
    drop(primary);
    let _ = std::fs::remove_dir_all(pdir);
}

#[test]
fn repl_token_on_primary_reports_live_per_shard_pairs() {
    let server = Server::start(1);
    let mut c = std::net::TcpStream::connect(("127.0.0.1", server.port)).unwrap();
    send_resp(&mut c, &[b"REPL.TOKEN"]);
    let before = read_int_array(&mut c);
    assert_eq!(before.len(), 2, "1 shard → 1 (gen, offset) pair");
    assert!(before[0] >= 1, "feed generation starts at ≥1, got {}", before[0]);
    send_resp(&mut c, &[b"SET", b"tok", b"v"]);
    assert_eq!(read_line(&mut c), b"+OK\r\n");
    send_resp(&mut c, &[b"REPL.TOKEN"]);
    let after = read_int_array(&mut c);
    assert_eq!(after[0], before[0], "generation unchanged by a plain write");
    assert!(
        after[1] > before[1],
        "next_offset must advance past the write: {} → {}",
        before[1],
        after[1]
    );
    server.shutdown();
}

#[test]
fn repl_wait_read_your_writes_and_future_token_misdirects() {
    let replication_base = kevy_chaos::pick_free_port().expect("repl port");
    let (primary, pport, pdir) = spawn_primary_process(replication_base);
    let replica = AttachedReplica::start(replication_base);
    let _gen = wait_replica_gen_learned(replica.port);

    let mut w = std::net::TcpStream::connect(("127.0.0.1", pport)).unwrap();
    let mut r = std::net::TcpStream::connect(("127.0.0.1", replica.port)).unwrap();

    // Read-your-writes rounds: write → token → REPL.WAIT +OK → GET
    // must see THE value written before the token, every round.
    for round in 0..10 {
        let val = format!("v{round}");
        send_resp(&mut w, &[b"SET", b"ryw", val.as_bytes()]);
        assert_eq!(read_line(&mut w), b"+OK\r\n");
        send_resp(&mut w, &[b"REPL.TOKEN"]);
        let tok = read_int_array(&mut w);
        assert_eq!(tok.len(), 2);
        let (g, off) = (tok[0].to_string(), tok[1].to_string());
        let _ = r.set_read_timeout(Some(std::time::Duration::from_secs(8)));
        send_resp(
            &mut r,
            &[b"REPL.WAIT", g.as_bytes(), off.as_bytes(), b"TIMEOUT", b"5000"],
        );
        let reply = read_line(&mut r);
        assert_eq!(
            reply,
            b"+OK\r\n",
            "round {round}: REPL.WAIT: {}",
            String::from_utf8_lossy(&reply)
        );
        send_resp(&mut r, &[b"GET", b"ryw"]);
        let header = read_line(&mut r);
        assert_eq!(header, format!("${}\r\n", val.len()).as_bytes(), "round {round}");
        let payload = read_line(&mut r);
        assert_eq!(payload, format!("{val}\r\n").as_bytes(), "round {round}");
    }

    // A token from the future (offset the primary never reached):
    // parks until TIMEOUT then -MISDIRECTED naming the upstream.
    send_resp(&mut w, &[b"REPL.TOKEN"]);
    let tok = read_int_array(&mut w);
    let (g, off) = (tok[0].to_string(), (tok[1] + 1000).to_string());
    let t0 = std::time::Instant::now();
    send_resp(
        &mut r,
        &[b"REPL.WAIT", g.as_bytes(), off.as_bytes(), b"TIMEOUT", b"300"],
    );
    let reply = read_line(&mut r);
    assert!(
        reply.starts_with(b"-MISDIRECTED writer is "),
        "future token must misdirect, got {}",
        String::from_utf8_lossy(&reply)
    );
    assert!(
        t0.elapsed() >= std::time::Duration::from_millis(250),
        "future token should park ~TIMEOUT before misdirecting"
    );

    // A wrong-generation token misdirects IMMEDIATELY (no park).
    let bad_gen = (tok[0] + 7).to_string();
    let off_now = tok[1].to_string();
    let t0 = std::time::Instant::now();
    send_resp(
        &mut r,
        &[b"REPL.WAIT", bad_gen.as_bytes(), off_now.as_bytes(), b"TIMEOUT", b"5000"],
    );
    let reply = read_line(&mut r);
    assert!(
        reply.starts_with(b"-MISDIRECTED"),
        "gen mismatch must misdirect, got {}",
        String::from_utf8_lossy(&reply)
    );
    assert!(t0.elapsed() < std::time::Duration::from_secs(1), "gen mismatch must not park");

    replica.shutdown();
    drop(primary);
    let _ = std::fs::remove_dir_all(pdir);
}

/// T8 offset-aliasing fence on the SERVER primary: after an unclean
/// restart the feed generation bumps and offsets restart at 0, so a
/// replica resuming with its old `(gen, offset)` cursor must get a
/// snapshot ship — never frame continuity, even when the new history
/// has grown past the old cursor (the shape where pre-fence code
/// silently served aliased frames).
#[test]
fn unclean_restart_generation_fence_ships_instead_of_aliasing() {
    // Boot 1 (fresh dir → feed gen 1): five writes → next_offset 5.
    let server = Server::start(1);
    let mut client = std::net::TcpStream::connect(("127.0.0.1", server.port)).unwrap();
    for i in 0..5 {
        send_resp(&mut client, &[b"SET", format!("old{i}").as_bytes(), b"v"]);
        assert_eq!(read_line(&mut client), b"+OK\r\n");
    }
    let probe = kevy_replicate::replica::ReplicaClient::connect(
        ("127.0.0.1", server.replication_base),
        "fence-probe",
        0,
    )
    .expect("probe handshake");
    let gen1 = probe.primary_gen_at_handshake();
    assert_eq!(gen1, 1, "fresh dir boots at feed generation 1");
    drop(probe);
    drop(client);

    // Unclean stop: keep the dir, delete the clean-shutdown
    // continuity marker so the next boot reads as unclean and bumps
    // the generation (feed_meta decision table).
    let dir = server.stop_take_dir();
    let meta = dir.path().join("feed-0.meta");
    let _ = std::fs::remove_file(&meta);

    // Boot 2 on the same dir: gen 2, offsets restart at 0. Race the
    // new history PAST the old cursor (10 > 5).
    let server = Server::start_in(1, dir);
    let mut client = std::net::TcpStream::connect(("127.0.0.1", server.port)).unwrap();
    for i in 0..10 {
        send_resp(&mut client, &[b"SET", format!("new{i}").as_bytes(), b"v"]);
        assert_eq!(read_line(&mut client), b"+OK\r\n");
    }

    // Old-history resume claim: (gen 1, offset 5). Pre-fence, the
    // pump would serve frames 5..10 of the NEW history — silently
    // missing new0..new4. The fence must ship a full snapshot.
    let mut replica = kevy_replicate::replica::ReplicaClient::connect_at(
        ("127.0.0.1", server.replication_base),
        "fence-probe",
        gen1,
        5,
        std::time::Duration::from_secs(5),
    )
    .expect("resume handshake");
    assert_eq!(
        replica.primary_gen_at_handshake(),
        gen1 + 1,
        "unclean restart must bump the feed generation"
    );
    // First non-ping event must be SnapshotBegin, not a live frame.
    loop {
        match replica.next_event().expect("event").expect("ok") {
            kevy_replicate::replica::ReplicaEvent::SnapshotBegin => break,
            kevy_replicate::replica::ReplicaEvent::Ping { .. } => continue,
            other => panic!("fence must ship, got {other:?}"),
        }
    }
    // And the ship must cover the WHOLE new history (ack = 10).
    let ack_offset = loop {
        match replica.next_event().expect("event").expect("ok") {
            kevy_replicate::replica::ReplicaEvent::SnapshotChunk(_) => continue,
            kevy_replicate::replica::ReplicaEvent::SnapshotEnd { ack_offset } => break ack_offset,
            kevy_replicate::replica::ReplicaEvent::Ping { .. } => continue,
            other => panic!("unexpected mid-ship event: {other:?}"),
        }
    };
    assert_eq!(ack_offset, 10, "snapshot covers all of the new history");
    server.shutdown();
}
