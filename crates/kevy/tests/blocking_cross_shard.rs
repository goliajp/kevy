//! Cross-shard BLOCK arbiter (v2-7e) — end-to-end against a real
//! multi-shard reactor. With `nshards = 8` a watched key is almost always
//! on a different shard than the connection that blocks on it, so these
//! exercise the cross-shard park / arm / wake / timeout protocol in
//! `kevy_rt::block_xshard` (the single-shard `blocking.rs` suite only ever
//! hits the in-shard fast path or the self-target arbiter).
//!
//! Before v2-7e a single-key `BLPOP` whose key lived on another shard than
//! the conn returned an empty 0-byte reply and hung the client forever —
//! `blpop_remote_key_*` are the regression tests for that.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

static START_GATE: Mutex<()> = Mutex::new(());

const NSHARDS: usize = 8;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
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
        out.extend_from_slice(&read_n(s, 1));
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
    fn start() -> Self {
        let _gate = START_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let port = free_port();
        let dir = std::env::temp_dir().join(format!(
            "kevy-xblock-{}",
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
            let rt = kevy_rt::Runtime::new([127, 0, 0, 1], port, NSHARDS, kevy::KevyCommands)
                .with_data_dir(dir_thread);
            rt.run(stop_thread).unwrap();
        });
        for _ in 0..400 {
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
        s.set_read_timeout(Some(std::time::Duration::from_secs(8)))
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

// ───────────── single-key, remote shard (the hang-bug fix) ─────────────

#[test]
fn blpop_remote_key_times_out_not_hang() {
    let srv = Server::start();
    // Several distinct keys: with 8 shards each is very likely on a shard
    // other than its conn's, so this hits the cross-shard park + timeout.
    for key in ["alpha", "bravo", "charlie", "delta", "echo"] {
        let mut c = srv.connect();
        c.write_all(&req(&[b"BLPOP", key.as_bytes(), b"0.2"])).unwrap();
        let t0 = std::time::Instant::now();
        let reply = read_reply(&mut c);
        assert_eq!(reply, b"*-1\r\n", "BLPOP {key} must time out with nil array, not hang");
        assert!(
            t0.elapsed() >= std::time::Duration::from_millis(80),
            "BLPOP {key} returned too early: {:?}",
            t0.elapsed()
        );
    }
}

#[test]
fn blpop_remote_key_woken_by_push() {
    let srv = Server::start();
    for key in ["foxtrot", "golf", "hotel", "india", "juliet"] {
        let mut consumer = srv.connect();
        let mut producer = srv.connect();
        consumer.write_all(&req(&[b"BLPOP", key.as_bytes(), b"5"])).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(40));
        producer.write_all(&req(&[b"LPUSH", key.as_bytes(), b"hi"])).unwrap();
        let _ = read_reply(&mut producer); // :1
        let reply = read_reply(&mut consumer);
        let expect = req_pop_reply(key, "hi");
        assert_eq!(reply, expect, "BLPOP {key} wake reply mismatch");
    }
}

#[test]
fn blpop_remote_key_immediate_hit() {
    let srv = Server::start();
    let mut c = srv.connect();
    c.write_all(&req(&[b"RPUSH", b"kilo", b"v"])).unwrap();
    let _ = read_reply(&mut c); // :1
    let mut b = srv.connect();
    b.write_all(&req(&[b"BLPOP", b"kilo", b"5"])).unwrap();
    let reply = read_reply(&mut b);
    assert_eq!(reply, req_pop_reply("kilo", "v"));
}

// ───────────── multi-key spanning shards ─────────────

#[test]
fn blpop_multi_key_woken_on_either_key() {
    let srv = Server::start();
    // 8 shards → `m1` and `m2` are very likely on different shards, and
    // both very likely remote to the consumer's shard.
    let mut consumer = srv.connect();
    let mut producer = srv.connect();
    consumer
        .write_all(&req(&[b"BLPOP", b"m1", b"m2", b"5"]))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(40));
    producer.write_all(&req(&[b"RPUSH", b"m2", b"got"])).unwrap();
    let _ = read_reply(&mut producer);
    let reply = read_reply(&mut consumer);
    assert_eq!(reply, req_pop_reply("m2", "got"));
}

#[test]
fn blpop_multi_key_all_empty_times_out() {
    let srv = Server::start();
    let mut c = srv.connect();
    c.write_all(&req(&[b"BLPOP", b"z1", b"z2", b"z3", b"0.2"]))
        .unwrap();
    let t0 = std::time::Instant::now();
    let reply = read_reply(&mut c);
    assert_eq!(reply, b"*-1\r\n");
    assert!(t0.elapsed() >= std::time::Duration::from_millis(80));
}

// ───────────── XREAD BLOCK, remote stream ─────────────

#[test]
fn xread_block_remote_stream_woken_by_xadd() {
    let srv = Server::start();
    let mut consumer = srv.connect();
    let mut producer = srv.connect();
    producer
        .write_all(&req(&[b"XADD", b"xs", b"1-0", b"f", b"v"]))
        .unwrap();
    let _ = read_reply(&mut producer);
    consumer
        .write_all(&req(&[b"XREAD", b"BLOCK", b"5000", b"STREAMS", b"xs", b"$"]))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(40));
    producer
        .write_all(&req(&[b"XADD", b"xs", b"2-0", b"f", b"v2"]))
        .unwrap();
    let _ = read_reply(&mut producer);
    let reply = read_reply(&mut consumer);
    assert!(reply.starts_with(b"*1\r\n"), "got {reply:?}");
    assert!(reply.windows(3).any(|w| w == b"2-0"));
    assert!(reply.windows(2).any(|w| w == b"v2"));
}

#[test]
fn xread_block_remote_stream_times_out() {
    let srv = Server::start();
    let mut p = srv.connect();
    p.write_all(&req(&[b"XADD", b"xt", b"1-0", b"f", b"v"])).unwrap();
    let _ = read_reply(&mut p);
    let mut c = srv.connect();
    c.write_all(&req(&[b"XREAD", b"BLOCK", b"150", b"STREAMS", b"xt", b"$"]))
        .unwrap();
    let t0 = std::time::Instant::now();
    let reply = read_reply(&mut c);
    assert_eq!(reply, b"$-1\r\n");
    assert!(t0.elapsed() >= std::time::Duration::from_millis(80));
}

// ───────────── disconnect during block ─────────────

#[test]
fn blpop_remote_disconnect_then_push_is_clean() {
    let srv = Server::start();
    // Consumer blocks on a remote key, then disconnects while parked — the
    // origin must broadcast cancel so the target drops the waiter.
    {
        let mut consumer = srv.connect();
        consumer.write_all(&req(&[b"BLPOP", b"dc", b"5"])).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(40));
        // drop = disconnect while blocked
    }
    std::thread::sleep(std::time::Duration::from_millis(40));
    // A later push must not be consumed by the gone waiter; the value stays
    // and a fresh BLPOP retrieves it.
    let mut producer = srv.connect();
    producer.write_all(&req(&[b"RPUSH", b"dc", b"stay"])).unwrap();
    assert_eq!(read_reply(&mut producer), b":1\r\n");
    let mut c2 = srv.connect();
    c2.write_all(&req(&[b"BLPOP", b"dc", b"5"])).unwrap();
    assert_eq!(read_reply(&mut c2), req_pop_reply("dc", "stay"));
}

/// `*2\r\n$<klen>\r\n<key>\r\n$<vlen>\r\n<val>\r\n` — BLPOP's wake reply.
fn req_pop_reply(key: &str, val: &str) -> Vec<u8> {
    format!(
        "*2\r\n${}\r\n{}\r\n${}\r\n{}\r\n",
        key.len(),
        key,
        val.len(),
        val
    )
    .into_bytes()
}
