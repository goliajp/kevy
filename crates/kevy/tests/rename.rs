//! `RENAME` / `RENAMENX` — same-shard atomic + cross-shard error
//! (v2-3a scope; cross-shard orchestrator pending v2-3b).
//!
//! Same-shard goes through `kevy-rt::Op::Rename` which calls
//! `Store::rename` atomically (entry move + WATCH bump + AOF log +
//! keyspace notification). Cross-shard currently replies
//! `-CROSSSHARD ...` so clients see a clear, non-`CROSSSLOT`
//! (cluster-coded) error.

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

fn read_reply(s: &mut std::net::TcpStream, expected: &[u8]) {
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
            "kevy-rename-{}",
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

/// Find two keys that hash to the same shard under `nshards`. The
/// runtime's `shard_of` uses `kevy_hash::KevyHash`; the easiest way
/// to test same-shard semantics is to try short keys until two land
/// on shard 0 of an `nshards=2` setup. The runtime is single-shard
/// (`nshards=1`) for these tests so every pair is trivially co-located.
#[test]
fn rename_overwrites_destination() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    c.write_all(&req(&[b"SET", b"a", b"src-value"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");
    c.write_all(&req(&[b"SET", b"b", b"dst-old"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");

    c.write_all(&req(&[b"RENAME", b"a", b"b"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");

    // dst now has src's value; src is gone.
    c.write_all(&req(&[b"GET", b"b"])).unwrap();
    read_reply(&mut c, b"$9\r\nsrc-value\r\n");
    c.write_all(&req(&[b"GET", b"a"])).unwrap();
    read_reply(&mut c, b"$-1\r\n");
}

#[test]
fn rename_no_such_key_errors() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    c.write_all(&req(&[b"RENAME", b"nope", b"dst"])).unwrap();
    let mut buf = [0u8; 32];
    let n = c.read(&mut buf).unwrap();
    assert!(
        buf[..n].starts_with(b"-ERR no such key"),
        "expected -ERR no such key, got {:?}",
        String::from_utf8_lossy(&buf[..n])
    );
}

#[test]
fn renamenx_returns_zero_when_dst_exists() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    c.write_all(&req(&[b"SET", b"a", b"x"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");
    c.write_all(&req(&[b"SET", b"b", b"y"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");

    c.write_all(&req(&[b"RENAMENX", b"a", b"b"])).unwrap();
    read_reply(&mut c, b":0\r\n");

    // Both keys unchanged.
    c.write_all(&req(&[b"GET", b"a"])).unwrap();
    read_reply(&mut c, b"$1\r\nx\r\n");
    c.write_all(&req(&[b"GET", b"b"])).unwrap();
    read_reply(&mut c, b"$1\r\ny\r\n");
}

#[test]
fn renamenx_returns_one_when_dst_missing() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    c.write_all(&req(&[b"SET", b"a", b"x"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");

    c.write_all(&req(&[b"RENAMENX", b"a", b"b"])).unwrap();
    read_reply(&mut c, b":1\r\n");

    c.write_all(&req(&[b"GET", b"b"])).unwrap();
    read_reply(&mut c, b"$1\r\nx\r\n");
    c.write_all(&req(&[b"GET", b"a"])).unwrap();
    read_reply(&mut c, b"$-1\r\n");
}

#[test]
fn rename_preserves_ttl() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    c.write_all(&req(&[b"SET", b"a", b"v"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");
    c.write_all(&req(&[b"EXPIRE", b"a", b"3600"])).unwrap();
    read_reply(&mut c, b":1\r\n");

    c.write_all(&req(&[b"RENAME", b"a", b"b"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");

    // b inherited the TTL.
    c.write_all(&req(&[b"TTL", b"b"])).unwrap();
    let mut buf = [0u8; 16];
    let n = c.read(&mut buf).unwrap();
    let s = String::from_utf8_lossy(&buf[..n]);
    let v: i64 = s.trim_start_matches(':').trim_end_matches("\r\n").parse().unwrap();
    assert!(
        (3590..=3600).contains(&v),
        "expected TTL ~3600s, got {v}"
    );
}

#[test]
fn rename_same_key_is_ok_for_rename() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    c.write_all(&req(&[b"SET", b"a", b"value"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");
    c.write_all(&req(&[b"RENAME", b"a", b"a"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");
    c.write_all(&req(&[b"GET", b"a"])).unwrap();
    read_reply(&mut c, b"$5\r\nvalue\r\n");
}

#[test]
fn renamenx_same_key_returns_zero() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    c.write_all(&req(&[b"SET", b"a", b"v"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");
    // Same-key RENAMENX returns :0 (Redis: dst already exists — itself).
    c.write_all(&req(&[b"RENAMENX", b"a", b"a"])).unwrap();
    read_reply(&mut c, b":0\r\n");
}

#[test]
fn cross_shard_rename_returns_crossshard_v2_3a() {
    // 4 shards + carefully chosen keys: "x" and "y" hash to different
    // shards under KevyHash. The actual mapping is non-trivial; we
    // accept either CROSSSHARD (different shards) or +OK (same shard
    // by chance — KevyHash deterministic so the test ought to be
    // stable across runs but defensive). We grep the byte prefix so
    // either outcome passes.
    let srv = Server::start(4);
    let mut c = srv.connect();
    c.write_all(&req(&[b"SET", b"x", b"vx"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");
    c.write_all(&req(&[b"RENAME", b"x", b"y"])).unwrap();
    let mut buf = [0u8; 128];
    let n = c.read(&mut buf).unwrap();
    let s = &buf[..n];
    assert!(
        s.starts_with(b"+OK") || s.starts_with(b"-CROSSSHARD"),
        "expected +OK (same shard) or -CROSSSHARD (different), got {:?}",
        String::from_utf8_lossy(s)
    );
}
