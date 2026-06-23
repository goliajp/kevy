//! `BZPOPMIN key [key ...] timeout` — blocking ZPOPMIN, the last
//! wire-level blocker for BullMQ worker dequeue. v1.27.3-dev.
//!
//! Coverage:
//! - eager pop with the lowest-scored member (single-key)
//! - timeout returns nil array (`*-1\r\n`) after ≥ ~ deadline
//! - multi-key: first key empty + second has data → returns from second
//! - negative timeout → ERR reply
//! - wrong-type key → WRONGTYPE reply
//! - wake-on-ZADD: parked BZPOPMIN unblocks when a sibling conn ZADDs
//!
//! All tests spin a real in-process kevy runtime + TCP socket so the
//! BlockHint resolve / arm / wake / pop chain is exercised end-to-end —
//! same harness shape as `tests/blocking.rs`.

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
        let _gate = START_GATE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let port = free_port();
        let dir = std::env::temp_dir().join(format!(
            "kevy-bzpopmin-{}",
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
        Self { port, dir, stop, handle: Some(handle) }
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

// ───────────── eager pop ─────────────

#[test]
fn bzpopmin_returns_lowest_scored_when_zset_has_members() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    // ZADD z 3 c 1 a 2 b — lowest is (a, 1).
    c.write_all(&req(&[b"ZADD", b"z", b"3", b"c", b"1", b"a", b"2", b"b"]))
        .unwrap();
    let _ = read_reply(&mut c); // :3
    c.write_all(&req(&[b"BZPOPMIN", b"z", b"5"])).unwrap();
    // *3\r\n$1\r\nz\r\n$1\r\na\r\n$1\r\n1\r\n
    let reply = read_reply(&mut c);
    assert_eq!(reply, b"*3\r\n$1\r\nz\r\n$1\r\na\r\n$1\r\n1\r\n");
    // The other two members are still there.
    c.write_all(&req(&[b"ZCARD", b"z"])).unwrap();
    let card = read_reply(&mut c);
    assert_eq!(card, b":2\r\n");
}

#[test]
fn bzpopmin_returns_float_score_via_fmt_score() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    c.write_all(&req(&[b"ZADD", b"z", b"1.5", b"a"])).unwrap();
    let _ = read_reply(&mut c);
    c.write_all(&req(&[b"BZPOPMIN", b"z", b"5"])).unwrap();
    let reply = read_reply(&mut c);
    assert_eq!(reply, b"*3\r\n$1\r\nz\r\n$1\r\na\r\n$3\r\n1.5\r\n");
}

// ───────────── timeout ─────────────

#[test]
fn bzpopmin_times_out_with_nil_array_when_zset_empty() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    // 100ms = 0.1s — keep test fast.
    c.write_all(&req(&[b"BZPOPMIN", b"empty", b"0.1"])).unwrap();
    let t0 = std::time::Instant::now();
    let reply = read_reply(&mut c);
    let elapsed = t0.elapsed();
    assert_eq!(reply, b"*-1\r\n", "BZPOPMIN timeout returns nil array");
    assert!(
        elapsed >= std::time::Duration::from_millis(80),
        "BZPOPMIN should block ≈ requested timeout (~100ms), got {elapsed:?}",
    );
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "BZPOPMIN timeout fired far too late ({elapsed:?})",
    );
}

// ───────────── multi-key ─────────────

#[test]
fn bzpopmin_multi_key_returns_from_first_non_empty() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    c.write_all(&req(&[b"ZADD", b"ready", b"7", b"x"])).unwrap();
    let _ = read_reply(&mut c); // :1
    // `missing` is empty; `ready` has data → arm-time readiness peek
    // resolves at once without parking.
    c.write_all(&req(&[b"BZPOPMIN", b"missing", b"ready", b"5"]))
        .unwrap();
    let reply = read_reply(&mut c);
    assert_eq!(reply, b"*3\r\n$5\r\nready\r\n$1\r\nx\r\n$1\r\n7\r\n");
}

#[test]
fn bzpopmin_multi_key_times_out_when_all_empty() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    c.write_all(&req(&[b"BZPOPMIN", b"a", b"b", b"0.1"])).unwrap();
    let t0 = std::time::Instant::now();
    let reply = read_reply(&mut c);
    assert_eq!(reply, b"*-1\r\n");
    assert!(t0.elapsed() >= std::time::Duration::from_millis(80));
}

// ───────────── errors ─────────────

#[test]
fn bzpopmin_negative_timeout_returns_err() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    c.write_all(&req(&[b"BZPOPMIN", b"k", b"-1"])).unwrap();
    let reply = read_reply(&mut c);
    assert!(
        reply.starts_with(b"-ERR"),
        "negative timeout must be an error, got {:?}",
        std::str::from_utf8(&reply).unwrap_or("<non-utf8>")
    );
}

#[test]
fn bzpopmin_non_numeric_timeout_returns_err() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    c.write_all(&req(&[b"BZPOPMIN", b"k", b"abc"])).unwrap();
    let reply = read_reply(&mut c);
    assert!(reply.starts_with(b"-ERR"));
}

#[test]
fn bzpopmin_wrong_type_key_returns_wrongtype() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    // A plain string at `s` — BZPOPMIN expects a sorted set.
    c.write_all(&req(&[b"SET", b"s", b"v"])).unwrap();
    let _ = read_reply(&mut c); // +OK
    c.write_all(&req(&[b"BZPOPMIN", b"s", b"5"])).unwrap();
    let reply = read_reply(&mut c);
    assert!(
        reply.starts_with(b"-WRONGTYPE"),
        "expected WRONGTYPE, got {:?}",
        std::str::from_utf8(&reply).unwrap_or("<non-utf8>")
    );
}

// ───────────── wake-on-ZADD ─────────────

#[test]
fn bzpopmin_woken_by_concurrent_zadd() {
    let srv = Server::start(1);
    let mut consumer = srv.connect();
    let mut producer = srv.connect();
    // Park consumer with a generous timeout — the ZADD wake must arrive first.
    consumer
        .write_all(&req(&[b"BZPOPMIN", b"wakeable", b"5"]))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    producer
        .write_all(&req(&[b"ZADD", b"wakeable", b"42", b"hello"]))
        .unwrap();
    let _ = read_reply(&mut producer); // :1
    let reply = read_reply(&mut consumer);
    assert_eq!(
        reply,
        b"*3\r\n$8\r\nwakeable\r\n$5\r\nhello\r\n$2\r\n42\r\n"
    );
}

#[test]
fn bzpopmin_woken_by_concurrent_zincrby_on_new_key() {
    let srv = Server::start(1);
    let mut consumer = srv.connect();
    let mut producer = srv.connect();
    consumer
        .write_all(&req(&[b"BZPOPMIN", b"newkey", b"5"]))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    // ZINCRBY on a missing key creates the zset with `incr` as the score.
    producer
        .write_all(&req(&[b"ZINCRBY", b"newkey", b"3", b"m"]))
        .unwrap();
    let _ = read_reply(&mut producer); // $1 3
    let reply = read_reply(&mut consumer);
    assert_eq!(reply, b"*3\r\n$6\r\nnewkey\r\n$1\r\nm\r\n$1\r\n3\r\n");
}

#[test]
fn bzpopmin_multi_key_woken_on_second_key() {
    let srv = Server::start(1);
    let mut consumer = srv.connect();
    let mut producer = srv.connect();
    consumer
        .write_all(&req(&[b"BZPOPMIN", b"z1", b"z2", b"5"]))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    // ZADD into the *second* watched key — the cross-shard arbiter
    // must serve z2.
    producer
        .write_all(&req(&[b"ZADD", b"z2", b"9", b"v2"]))
        .unwrap();
    let _ = read_reply(&mut producer); // :1
    let reply = read_reply(&mut consumer);
    assert_eq!(reply, b"*3\r\n$2\r\nz2\r\n$2\r\nv2\r\n$1\r\n9\r\n");
}
