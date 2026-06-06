//! `BLPOP` / `BRPOP` / `XREAD BLOCK` / `XREADGROUP BLOCK` —
//! end-to-end tests against a real reactor + socket. Verifies the
//! v2-7d BLOCK reactor's three core paths per command:
//!
//! 1. **Hit immediately** — non-empty list / fresh stream entry already
//!    available → the command returns at once without parking the conn.
//! 2. **Timeout** — empty, `BLOCK ms` elapses → the reactor's tick fires
//!    a nil reply (shape per command: `*-1` for BLPOP/BRPOP, `$-1` for
//!    XREAD/XREADGROUP) and unblocks the conn.
//! 3. **Wake** — empty + a sibling conn pushes / XADDs the watched key
//!    → the oldest waiter is popped, its command replays, and the
//!    reply lands on the parked conn.

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

fn read_n(s: &mut std::net::TcpStream, n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    s.read_exact(&mut buf).unwrap();
    buf
}

fn read_line(s: &mut std::net::TcpStream, out: &mut Vec<u8>) {
    loop {
        let b = read_n(s, 1);
        out.extend_from_slice(&b);
        if out.ends_with(b"\r\n") {
            break;
        }
    }
}

fn read_len(s: &mut std::net::TcpStream, out: &mut Vec<u8>) -> i64 {
    let start = out.len();
    read_line(s, out);
    let line = &out[start..out.len() - 2];
    std::str::from_utf8(line).unwrap().parse().unwrap()
}

fn read_reply(s: &mut std::net::TcpStream) -> Vec<u8> {
    let head = read_n(s, 1);
    let mut out = head.clone();
    match head[0] {
        b'+' | b'-' | b':' => read_line(s, &mut out),
        b'$' => {
            let len = read_len(s, &mut out);
            if len < 0 {
                return out;
            }
            out.extend_from_slice(&read_n(s, len as usize + 2));
        }
        b'*' => {
            let n = read_len(s, &mut out);
            if n < 0 {
                return out;
            }
            for _ in 0..n {
                out.extend_from_slice(&read_reply(s));
            }
        }
        other => panic!("unknown reply prefix {other:?}"),
    }
    out
}

struct Server {
    port: u16,
    dir: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start(nshards: usize) -> Self {
        let _gate = START_GATE.lock().unwrap_or_else(|e| e.into_inner());
        let port = free_port();
        let dir = std::env::temp_dir().join(format!(
            "kevy-blocking-{}",
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
                .with_data_dir(dir_thread);
            rt.run(stop_thread).unwrap();
        });
        for _ in 0..200 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
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

// ───────────── BLPOP ─────────────

#[test]
fn blpop_returns_immediately_when_list_has_value() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    c.write_all(&req(&[b"RPUSH", b"k", b"v"])).unwrap();
    let _ = read_reply(&mut c); // :1
    c.write_all(&req(&[b"BLPOP", b"k", b"5"])).unwrap();
    // Expect: *2\r\n$1\r\nk\r\n$1\r\nv\r\n
    let reply = read_reply(&mut c);
    assert_eq!(reply, b"*2\r\n$1\r\nk\r\n$1\r\nv\r\n");
}

#[test]
fn blpop_times_out_with_nil_array_when_list_empty() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    // 100ms = 0.1s — short enough that the test stays fast.
    c.write_all(&req(&[b"BLPOP", b"empty", b"0.1"])).unwrap();
    let t0 = std::time::Instant::now();
    let reply = read_reply(&mut c);
    let elapsed = t0.elapsed();
    assert_eq!(reply, b"*-1\r\n", "BLPOP timeout must return nil array");
    assert!(
        elapsed >= std::time::Duration::from_millis(80),
        "BLPOP should block at least the requested timeout (~100ms), got {elapsed:?}",
    );
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "BLPOP timeout fired far too late ({elapsed:?})",
    );
}

#[test]
fn blpop_woken_by_concurrent_push() {
    let srv = Server::start(1);
    let mut consumer = srv.connect();
    let mut producer = srv.connect();
    // Park the consumer with a generous timeout — wake must come first.
    consumer
        .write_all(&req(&[b"BLPOP", b"wakeable", b"5"]))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    producer
        .write_all(&req(&[b"LPUSH", b"wakeable", b"hello"]))
        .unwrap();
    let _push_reply = read_reply(&mut producer); // :1
    let reply = read_reply(&mut consumer);
    assert_eq!(reply, b"*2\r\n$8\r\nwakeable\r\n$5\r\nhello\r\n");
}

// ───────────── BRPOP ─────────────

#[test]
fn brpop_returns_immediately_when_list_has_value() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    c.write_all(&req(&[b"RPUSH", b"k", b"v"])).unwrap();
    let _ = read_reply(&mut c);
    c.write_all(&req(&[b"BRPOP", b"k", b"5"])).unwrap();
    let reply = read_reply(&mut c);
    assert_eq!(reply, b"*2\r\n$1\r\nk\r\n$1\r\nv\r\n");
}

#[test]
fn brpop_woken_by_concurrent_rpush() {
    let srv = Server::start(1);
    let mut consumer = srv.connect();
    let mut producer = srv.connect();
    consumer.write_all(&req(&[b"BRPOP", b"q", b"5"])).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    producer.write_all(&req(&[b"RPUSH", b"q", b"x"])).unwrap();
    let _ = read_reply(&mut producer);
    let reply = read_reply(&mut consumer);
    assert_eq!(reply, b"*2\r\n$1\r\nq\r\n$1\r\nx\r\n");
}

// ───────────── XREAD BLOCK ─────────────

#[test]
fn xread_block_returns_immediately_when_stream_has_entry() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    c.write_all(&req(&[b"XADD", b"s", b"1-0", b"f", b"v"])).unwrap();
    let _ = read_reply(&mut c); // $3 1-0
    c.write_all(&req(&[
        b"XREAD", b"BLOCK", b"5000", b"STREAMS", b"s", b"0",
    ])).unwrap();
    // Expect *1 [*2 s [*1 [*2 1-0 [*2 f v]]]]
    let reply = read_reply(&mut c);
    // Quick sanity: starts with *1 and contains the entry payload.
    assert!(reply.starts_with(b"*1\r\n"), "expected one stream in reply, got {reply:?}");
    assert!(reply.windows(3).any(|w| w == b"1-0"), "expected entry id 1-0 in reply");
    assert!(reply.windows(1).any(|w| w == b"v"), "expected value v in reply");
}

#[test]
fn xread_block_times_out_with_nil_bulk_when_no_entries() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    // Create the stream so `$` resolves; otherwise xread_dollar_last_id
    // errors out before BLOCK can engage.
    c.write_all(&req(&[b"XADD", b"s", b"1-0", b"f", b"v"])).unwrap();
    let _ = read_reply(&mut c);
    c.write_all(&req(&[
        b"XREAD", b"BLOCK", b"100", b"STREAMS", b"s", b"$",
    ])).unwrap();
    let t0 = std::time::Instant::now();
    let reply = read_reply(&mut c);
    let elapsed = t0.elapsed();
    assert_eq!(reply, b"$-1\r\n", "XREAD BLOCK timeout must return nil bulk");
    assert!(elapsed >= std::time::Duration::from_millis(80));
}

#[test]
fn xread_block_woken_by_concurrent_xadd() {
    // Wake path with an explicit cursor (no `$`). Companion to
    // `xread_block_dollar_id_wakes` below, which exercises the same
    // path with the `$` cursor that needs park-time rewriting.
    let srv = Server::start(1);
    let mut consumer = srv.connect();
    let mut producer = srv.connect();
    producer
        .write_all(&req(&[b"XADD", b"stream", b"1-0", b"f", b"v"]))
        .unwrap();
    let _ = read_reply(&mut producer);
    consumer
        .write_all(&req(&[
            b"XREAD", b"BLOCK", b"5000", b"STREAMS", b"stream", b"1-0",
        ]))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    producer
        .write_all(&req(&[b"XADD", b"stream", b"2-0", b"f", b"v2"]))
        .unwrap();
    let _ = read_reply(&mut producer);
    let reply = read_reply(&mut consumer);
    assert!(reply.starts_with(b"*1\r\n"));
    assert!(reply.windows(3).any(|w| w == b"2-0"));
    assert!(reply.windows(2).any(|w| w == b"v2"));
}

#[test]
fn xread_block_dollar_id_wakes() {
    // `$` cursor: park-time rewrite (Commands::resolve_block_argv on
    // BlockKind::XReadBlock) must snapshot the stream's last_id when
    // the conn is registered, so the wake retry sees the original
    // cursor — not the post-XADD last_id, which would mean "0 entries
    // > last_id" and a timeout. Regression test for v2-7e.
    let srv = Server::start(1);
    let mut consumer = srv.connect();
    let mut producer = srv.connect();
    // Pre-populate so `$` resolves to a real ID (xread_dollar_last_id
    // errors on a missing key, which would prevent registration).
    producer
        .write_all(&req(&[b"XADD", b"stream", b"1-0", b"f", b"v"]))
        .unwrap();
    let _ = read_reply(&mut producer);
    consumer
        .write_all(&req(&[
            b"XREAD", b"BLOCK", b"5000", b"STREAMS", b"stream", b"$",
        ]))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    producer
        .write_all(&req(&[b"XADD", b"stream", b"2-0", b"f", b"v2"]))
        .unwrap();
    let _ = read_reply(&mut producer);
    let reply = read_reply(&mut consumer);
    assert!(
        reply.starts_with(b"*1\r\n"),
        "expected one stream in reply, got {:?}",
        std::str::from_utf8(&reply).unwrap_or("<non-utf8>")
    );
    assert!(reply.windows(3).any(|w| w == b"2-0"));
    assert!(reply.windows(2).any(|w| w == b"v2"));
}

// ───────────── XREADGROUP BLOCK ─────────────

#[test]
fn xreadgroup_block_times_out_when_no_new_entries() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    // Set up: create stream + group.
    c.write_all(&req(&[b"XADD", b"s", b"1-0", b"f", b"v"])).unwrap();
    let _ = read_reply(&mut c);
    c.write_all(&req(&[b"XGROUP", b"CREATE", b"s", b"g", b"$"])).unwrap();
    let _ = read_reply(&mut c); // +OK
    c.write_all(&req(&[
        b"XREADGROUP",
        b"GROUP",
        b"g",
        b"alice",
        b"BLOCK",
        b"100",
        b"STREAMS",
        b"s",
        b">",
    ])).unwrap();
    let t0 = std::time::Instant::now();
    let reply = read_reply(&mut c);
    let elapsed = t0.elapsed();
    assert_eq!(reply, b"$-1\r\n", "XREADGROUP BLOCK timeout returns nil bulk");
    assert!(elapsed >= std::time::Duration::from_millis(80));
}

#[test]
fn xreadgroup_block_woken_by_concurrent_xadd() {
    let srv = Server::start(1);
    let mut consumer = srv.connect();
    let mut producer = srv.connect();
    producer.write_all(&req(&[b"XADD", b"stream2", b"1-0", b"f", b"v"])).unwrap();
    let _ = read_reply(&mut producer);
    producer.write_all(&req(&[b"XGROUP", b"CREATE", b"stream2", b"g", b"$"])).unwrap();
    let _ = read_reply(&mut producer);
    consumer.write_all(&req(&[
        b"XREADGROUP",
        b"GROUP",
        b"g",
        b"bob",
        b"BLOCK",
        b"5000",
        b"STREAMS",
        b"stream2",
        b">",
    ])).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    producer.write_all(&req(&[b"XADD", b"stream2", b"2-0", b"f", b"v2"])).unwrap();
    let _ = read_reply(&mut producer);
    let reply = read_reply(&mut consumer);
    assert!(reply.starts_with(b"*1\r\n"));
    assert!(reply.windows(3).any(|w| w == b"2-0"));
    assert!(reply.windows(2).any(|w| w == b"v2"));
}

// ───────────── Multi-key rejection ─────────────

#[test]
fn blpop_multi_key_returns_explicit_error_not_nil() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    c.write_all(&req(&[b"BLPOP", b"a", b"b", b"0.1"])).unwrap();
    let reply = read_reply(&mut c);
    assert!(reply.starts_with(b"-ERR"), "expected error, got {reply:?}");
    let msg = std::str::from_utf8(&reply).unwrap();
    assert!(msg.contains("multi-key"), "error should mention multi-key: {msg}");
}
