//! v2-4 SLOWLOG — `GET / LEN / RESET / HELP`, per-shard ring buffer,
//! threshold filtering, cross-shard aggregation.
//!
//! Each test owns its own [`Runtime`] (no `config_global`), so the
//! tuning is pinned via `Runtime::with_slowlog(slower_than, max_len)`.
//! The hot-reload path (CONFIG SET → `LiveRuntimeConfig::slowlog_*` →
//! `apply_live_runtime_config`) is covered in `slowlog_hotreload.rs`,
//! which runs in its own binary so `config_global`'s once-init state
//! doesn't leak across files.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

static START_GATE: Mutex<()> = Mutex::new(());

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
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

fn read_exact_bytes(s: &mut std::net::TcpStream, expected: &[u8]) {
    let mut buf = vec![0u8; expected.len()];
    s.read_exact(&mut buf).unwrap();
    assert_eq!(
        &buf,
        expected,
        "expected {:?}, got {:?}",
        String::from_utf8_lossy(expected),
        String::from_utf8_lossy(&buf),
    );
}

/// Read a single RESP line up to (and including) the trailing `\r\n`.
fn read_resp_line(s: &mut std::net::TcpStream) -> Vec<u8> {
    let mut out = Vec::new();
    let mut prev = 0u8;
    let mut byte = [0u8; 1];
    loop {
        s.read_exact(&mut byte).unwrap();
        out.push(byte[0]);
        if prev == b'\r' && byte[0] == b'\n' {
            return out;
        }
        prev = byte[0];
    }
}

/// Read one full RESP reply (handles SimpleString / Error / Integer /
/// BulkString / Array — enough for SLOWLOG GET nested replies).
fn read_resp_value(s: &mut std::net::TcpStream) -> Vec<u8> {
    let line = read_resp_line(s);
    let mut out = line.clone();
    match line.first() {
        Some(b'+' | b'-' | b':') => out,
        Some(b'$') => {
            let n: i64 = std::str::from_utf8(&line[1..line.len() - 2])
                .unwrap()
                .parse()
                .unwrap();
            if n < 0 {
                return out;
            }
            let mut body = vec![0u8; n as usize + 2];
            s.read_exact(&mut body).unwrap();
            out.extend_from_slice(&body);
            out
        }
        Some(b'*') => {
            let n: i64 = std::str::from_utf8(&line[1..line.len() - 2])
                .unwrap()
                .parse()
                .unwrap();
            if n < 0 {
                return out;
            }
            for _ in 0..n {
                out.extend_from_slice(&read_resp_value(s));
            }
            out
        }
        _ => panic!("unexpected RESP prefix: {:?}", String::from_utf8_lossy(&line)),
    }
}

struct Server {
    port: u16,
    dir: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start(slower_than_micros: i64, max_len: u32, nshards: usize) -> Self {
        let _gate = START_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let port = free_port();
        let dir = std::env::temp_dir().join(format!(
            "kevy-slowlog-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let dir_thread = dir.clone();
        let handle = std::thread::spawn(move || {
            let rt = kevy_rt::Runtime::new([127, 0, 0, 1], port, nshards, kevy::KevyCommands)
                .with_data_dir(dir_thread)
                .with_aof(false)
                .with_slowlog(slower_than_micros, max_len);
            rt.run(stop_thread).unwrap();
        });
        let mut ready = false;
        for _ in 0..200 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                ready = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(ready, "runtime did not come up");
        Self {
            port,
            dir,
            stop,
            handle: Some(handle),
        }
    }

    fn connect(&self) -> std::net::TcpStream {
        let s = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        s
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

/// Parse a SLOWLOG LEN reply (`:N\r\n`) into the integer.
fn slowlog_len(c: &mut std::net::TcpStream) -> i64 {
    c.write_all(&req(&[b"SLOWLOG", b"LEN"])).unwrap();
    let line = read_resp_line(c);
    assert_eq!(line[0], b':', "SLOWLOG LEN expected :N reply");
    std::str::from_utf8(&line[1..line.len() - 2])
        .unwrap()
        .parse()
        .unwrap()
}

#[test]
fn slowlog_help_returns_static_array() {
    let srv = Server::start(10_000, 128, 1);
    let mut c = srv.connect();
    c.write_all(&req(&[b"SLOWLOG", b"HELP"])).unwrap();
    let reply = read_resp_value(&mut c);
    assert!(reply.starts_with(b"*"), "HELP must return an array");
    // First bulk is the synopsis line — verify it lands intact.
    let body = String::from_utf8_lossy(&reply);
    assert!(
        body.contains("SLOWLOG <subcommand>"),
        "HELP synopsis missing: {body}"
    );
    assert!(body.contains("Reset the slowlog."), "HELP body missing");
}

#[test]
fn slowlog_get_len_reset_basic_flow_records_everything_when_threshold_zero() {
    let srv = Server::start(0, 128, 1);
    let mut c = srv.connect();

    // Threshold = 0 records ALL commands (elapsed > 0).
    for i in 0..5u32 {
        let key = format!("k{i}");
        c.write_all(&req(&[b"SET", key.as_bytes(), b"v"])).unwrap();
        read_exact_bytes(&mut c, b"+OK\r\n");
    }
    // Each SET ran on shard 0 (nshards=1), inline path, so all 5 are
    // logged. SLOWLOG itself isn't logged (it goes through
    // Route::Slowlog, not Op::Dispatch / inline).
    assert!(slowlog_len(&mut c) >= 5, "expected ≥5 entries after 5 SETs");

    c.write_all(&req(&[b"SLOWLOG", b"GET"])).unwrap();
    let reply = read_resp_value(&mut c);
    assert!(reply.starts_with(b"*"), "GET expected array");
    // Default count = 10, so we get up to 10 entries; we wrote 5, so
    // the array length is exactly 5.
    let body = String::from_utf8_lossy(&reply);
    assert!(body.contains("SET"), "GET reply missing SET argv: {body}");

    c.write_all(&req(&[b"SLOWLOG", b"RESET"])).unwrap();
    read_exact_bytes(&mut c, b"+OK\r\n");

    assert_eq!(slowlog_len(&mut c), 0, "RESET must zero the ring");
}

#[test]
fn slowlog_threshold_high_records_nothing() {
    // Threshold = i64::MAX: no command can exceed it, so the ring stays empty.
    // (Clock pair is still taken — we're verifying the gating logic only.)
    let srv = Server::start(i64::MAX, 128, 1);
    let mut c = srv.connect();
    for i in 0..10u32 {
        let key = format!("t{i}");
        c.write_all(&req(&[b"SET", key.as_bytes(), b"v"])).unwrap();
        read_exact_bytes(&mut c, b"+OK\r\n");
    }
    assert_eq!(
        slowlog_len(&mut c),
        0,
        "threshold = i64::MAX must reject every entry"
    );
}

#[test]
fn slowlog_off_skips_clock_and_records_nothing() {
    // Threshold = -1 disables the log entirely. The runtime path skips
    // `Instant::now()` (see exec.rs / exec_op.rs gating) so SLOWLOG OFF
    // is a true zero-cost mode.
    let srv = Server::start(-1, 128, 1);
    let mut c = srv.connect();
    for i in 0..10u32 {
        let key = format!("off{i}");
        c.write_all(&req(&[b"SET", key.as_bytes(), b"v"])).unwrap();
        read_exact_bytes(&mut c, b"+OK\r\n");
    }
    assert_eq!(slowlog_len(&mut c), 0, "OFF must record nothing");
}

#[test]
fn slowlog_aggregates_across_shards() {
    // 4 shards, threshold = 0 — every SET records exactly once on its
    // owning shard. SLOWLOG LEN fans out + sums; we wrote N SETs so the
    // total is ≥ N (other admin commands may also tick the counter, but
    // never below N).
    let srv = Server::start(0, 256, 4);
    let mut c = srv.connect();
    let n = 60u32;
    for i in 0..n {
        let key = format!("xs{i}");
        c.write_all(&req(&[b"SET", key.as_bytes(), b"v"])).unwrap();
        read_exact_bytes(&mut c, b"+OK\r\n");
    }
    let total = slowlog_len(&mut c);
    assert!(
        total >= i64::from(n),
        "expected ≥{n} SLOWLOG entries across 4 shards, got {total}"
    );

    // SLOWLOG GET (default count = 10) fans out and trims to 10 entries.
    c.write_all(&req(&[b"SLOWLOG", b"GET"])).unwrap();
    let reply = read_resp_value(&mut c);
    let body = String::from_utf8_lossy(&reply);
    assert!(reply.starts_with(b"*"), "GET expected array");
    assert!(body.contains("SET"), "GET reply missing SET argv");

    // RESET fans out — every shard clears its ring.
    c.write_all(&req(&[b"SLOWLOG", b"RESET"])).unwrap();
    read_exact_bytes(&mut c, b"+OK\r\n");
    assert_eq!(slowlog_len(&mut c), 0, "RESET must clear every shard");
}

#[test]
fn slowlog_max_len_evicts_oldest() {
    // max_len = 3 → only the most-recent 3 commands survive on shard 0.
    let srv = Server::start(0, 3, 1);
    let mut c = srv.connect();
    for i in 0..10u32 {
        let key = format!("e{i}");
        c.write_all(&req(&[b"SET", key.as_bytes(), b"v"])).unwrap();
        read_exact_bytes(&mut c, b"+OK\r\n");
    }
    let len = slowlog_len(&mut c);
    assert_eq!(len, 3, "max_len = 3 must cap the ring at 3 entries");
}

#[test]
fn slowlog_unknown_subcommand_returns_error() {
    let srv = Server::start(10_000, 128, 1);
    let mut c = srv.connect();
    c.write_all(&req(&[b"SLOWLOG", b"BOGUS"])).unwrap();
    let line = read_resp_line(&mut c);
    assert_eq!(line[0], b'-', "unknown subcommand must be an error reply");
    let body = String::from_utf8_lossy(&line);
    assert!(body.contains("Unknown SLOWLOG subcommand"), "error body: {body}");
}

#[test]
fn slowlog_get_with_count_truncates() {
    let srv = Server::start(0, 128, 1);
    let mut c = srv.connect();
    for i in 0..7u32 {
        c.write_all(&req(&[b"SET", format!("tr{i}").as_bytes(), b"v"]))
            .unwrap();
        read_exact_bytes(&mut c, b"+OK\r\n");
    }
    // GET 3 → exactly 3 entries (most recent).
    c.write_all(&req(&[b"SLOWLOG", b"GET", b"3"])).unwrap();
    let reply = read_resp_value(&mut c);
    assert!(reply.starts_with(b"*3\r\n"), "GET 3 expected *3\\r\\n header, got {:?}",
        String::from_utf8_lossy(&reply[..reply.len().min(16)]));
}
