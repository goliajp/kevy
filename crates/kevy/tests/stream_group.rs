//! Consumer-group commands (sprint B of v2-7).

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
        other => panic!("unknown prefix {other:?}: {out:?}"),
    }
    out
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
            "kevy-stream-group-{}",
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

fn seed_three(c: &mut std::net::TcpStream) {
    for id in ["1-0", "2-0", "3-0"] {
        c.write_all(&req(&[b"XADD", b"s", id.as_bytes(), b"f", b"v"])).unwrap();
        let _ = read_reply(c);
    }
}

// ───────────── XGROUP ─────────────

#[test]
fn xgroup_create_at_id_returns_ok() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    seed_three(&mut c);
    c.write_all(&req(&[b"XGROUP", b"CREATE", b"s", b"g1", b"0"])).unwrap();
    assert_eq!(read_reply(&mut c), b"+OK\r\n");
}

#[test]
fn xgroup_create_dollar_means_current_last_id() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    seed_three(&mut c);
    c.write_all(&req(&[b"XGROUP", b"CREATE", b"s", b"g1", b"$"])).unwrap();
    assert_eq!(read_reply(&mut c), b"+OK\r\n");
    // No entries delivered to a fresh `$` consumer.
    c.write_all(&req(&[
        b"XREADGROUP", b"GROUP", b"g1", b"c1", b"STREAMS", b"s", b">",
    ]))
    .unwrap();
    assert_eq!(read_reply(&mut c), b"*-1\r\n");
}

#[test]
fn xgroup_create_busygroup_when_duplicate() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    seed_three(&mut c);
    c.write_all(&req(&[b"XGROUP", b"CREATE", b"s", b"g1", b"0"])).unwrap();
    let _ = read_reply(&mut c);
    c.write_all(&req(&[b"XGROUP", b"CREATE", b"s", b"g1", b"0"])).unwrap();
    let r = read_reply(&mut c);
    assert!(r.starts_with(b"-BUSYGROUP"), "got: {:?}", String::from_utf8_lossy(&r));
}

#[test]
fn xgroup_create_mkstream_creates_missing_key() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    c.write_all(&req(&[
        b"XGROUP", b"CREATE", b"s", b"g1", b"$", b"MKSTREAM",
    ]))
    .unwrap();
    assert_eq!(read_reply(&mut c), b"+OK\r\n");
    c.write_all(&req(&[b"EXISTS", b"s"])).unwrap();
    assert_eq!(read_reply(&mut c), b":1\r\n");
}

#[test]
fn xgroup_destroy_returns_one_then_zero() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    seed_three(&mut c);
    c.write_all(&req(&[b"XGROUP", b"CREATE", b"s", b"g1", b"0"])).unwrap();
    let _ = read_reply(&mut c);
    c.write_all(&req(&[b"XGROUP", b"DESTROY", b"s", b"g1"])).unwrap();
    assert_eq!(read_reply(&mut c), b":1\r\n");
    c.write_all(&req(&[b"XGROUP", b"DESTROY", b"s", b"g1"])).unwrap();
    assert_eq!(read_reply(&mut c), b":0\r\n");
}

// ───────────── XREADGROUP ─────────────

fn setup_group(c: &mut std::net::TcpStream) {
    seed_three(c);
    c.write_all(&req(&[b"XGROUP", b"CREATE", b"s", b"g1", b"0"])).unwrap();
    let _ = read_reply(c);
}

#[test]
fn xreadgroup_with_new_returns_unread_and_grows_pel() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    setup_group(&mut c);
    c.write_all(&req(&[
        b"XREADGROUP", b"GROUP", b"g1", b"c1", b"STREAMS", b"s", b">",
    ]))
    .unwrap();
    let r = read_reply(&mut c);
    let s = String::from_utf8_lossy(&r);
    assert!(s.contains("1-0") && s.contains("2-0") && s.contains("3-0"), "got: {s}");
    // PEL grew by 3
    c.write_all(&req(&[b"XPENDING", b"s", b"g1"])).unwrap();
    let p = read_reply(&mut c);
    let ps = String::from_utf8_lossy(&p);
    assert!(ps.starts_with("*4\r\n:3\r\n"), "expected total 3: {ps}");
}

#[test]
fn xreadgroup_explicit_id_replays_pel() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    setup_group(&mut c);
    // First read with > delivers entries → builds PEL for c1.
    c.write_all(&req(&[
        b"XREADGROUP", b"GROUP", b"g1", b"c1", b"STREAMS", b"s", b">",
    ]))
    .unwrap();
    let _ = read_reply(&mut c);
    // Replay PEL with explicit ID = 0 → re-deliver all entries.
    c.write_all(&req(&[
        b"XREADGROUP", b"GROUP", b"g1", b"c1", b"STREAMS", b"s", b"0",
    ]))
    .unwrap();
    let r = read_reply(&mut c);
    let s = String::from_utf8_lossy(&r);
    assert!(s.contains("1-0") && s.contains("3-0"), "replay: {s}");
}

#[test]
fn xreadgroup_noack_skips_pel() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    setup_group(&mut c);
    c.write_all(&req(&[
        b"XREADGROUP", b"GROUP", b"g1", b"c1", b"NOACK", b"STREAMS", b"s", b">",
    ]))
    .unwrap();
    let _ = read_reply(&mut c);
    c.write_all(&req(&[b"XPENDING", b"s", b"g1"])).unwrap();
    let p = read_reply(&mut c);
    assert!(
        String::from_utf8_lossy(&p).starts_with("*4\r\n:0\r\n"),
        "NOACK should leave PEL empty: {:?}",
        String::from_utf8_lossy(&p),
    );
}

// ───────────── XACK ─────────────

#[test]
fn xack_drops_from_pel() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    setup_group(&mut c);
    c.write_all(&req(&[
        b"XREADGROUP", b"GROUP", b"g1", b"c1", b"STREAMS", b"s", b">",
    ]))
    .unwrap();
    let _ = read_reply(&mut c);
    c.write_all(&req(&[b"XACK", b"s", b"g1", b"1-0", b"2-0"])).unwrap();
    assert_eq!(read_reply(&mut c), b":2\r\n");
    c.write_all(&req(&[b"XPENDING", b"s", b"g1"])).unwrap();
    let p = read_reply(&mut c);
    assert!(
        String::from_utf8_lossy(&p).starts_with("*4\r\n:1\r\n"),
        "1 entry should remain in PEL: {:?}",
        String::from_utf8_lossy(&p),
    );
}

// ───────────── XPENDING ─────────────

#[test]
fn xpending_extended_form_lists_rows() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    setup_group(&mut c);
    c.write_all(&req(&[
        b"XREADGROUP", b"GROUP", b"g1", b"c1", b"STREAMS", b"s", b">",
    ]))
    .unwrap();
    let _ = read_reply(&mut c);
    c.write_all(&req(&[
        b"XPENDING", b"s", b"g1", b"-", b"+", b"10",
    ]))
    .unwrap();
    let r = read_reply(&mut c);
    let s = String::from_utf8_lossy(&r);
    assert!(s.starts_with("*3\r\n"), "expected 3 rows: {s}");
    assert!(s.contains("1-0") && s.contains("c1"));
}

// ───────────── XCLAIM ─────────────

#[test]
fn xclaim_transfers_ownership() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    setup_group(&mut c);
    c.write_all(&req(&[
        b"XREADGROUP", b"GROUP", b"g1", b"c1", b"STREAMS", b"s", b">",
    ]))
    .unwrap();
    let _ = read_reply(&mut c);
    // min-idle-ms = 0 → always claim
    c.write_all(&req(&[
        b"XCLAIM", b"s", b"g1", b"c2", b"0", b"1-0", b"2-0",
    ]))
    .unwrap();
    let r = read_reply(&mut c);
    let s = String::from_utf8_lossy(&r);
    assert!(s.starts_with("*2\r\n"), "claimed 2 entries: {s}");
    assert!(s.contains("1-0") && s.contains("2-0"));
    // XPENDING per consumer
    c.write_all(&req(&[
        b"XPENDING", b"s", b"g1", b"-", b"+", b"10", b"c1",
    ]))
    .unwrap();
    let p = read_reply(&mut c);
    let ps = String::from_utf8_lossy(&p);
    assert!(ps.starts_with("*1\r\n"), "c1 should keep only 3-0: {ps}");
    assert!(ps.contains("3-0") && !ps.contains("1-0"));
}

#[test]
fn xclaim_justid_returns_id_array() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    setup_group(&mut c);
    c.write_all(&req(&[
        b"XREADGROUP", b"GROUP", b"g1", b"c1", b"STREAMS", b"s", b">",
    ]))
    .unwrap();
    let _ = read_reply(&mut c);
    c.write_all(&req(&[
        b"XCLAIM", b"s", b"g1", b"c2", b"0", b"1-0", b"JUSTID",
    ]))
    .unwrap();
    let r = read_reply(&mut c);
    assert_eq!(r, b"*1\r\n$3\r\n1-0\r\n");
}

// ───────────── XAUTOCLAIM ─────────────

#[test]
fn xautoclaim_walks_pel_and_returns_cursor() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    setup_group(&mut c);
    c.write_all(&req(&[
        b"XREADGROUP", b"GROUP", b"g1", b"c1", b"STREAMS", b"s", b">",
    ]))
    .unwrap();
    let _ = read_reply(&mut c);
    c.write_all(&req(&[
        b"XAUTOCLAIM", b"s", b"g1", b"c2", b"0", b"0", b"COUNT", b"2",
    ]))
    .unwrap();
    let r = read_reply(&mut c);
    let s = String::from_utf8_lossy(&r);
    // 3-element array: cursor, claimed entries, deleted ids
    assert!(s.starts_with("*3\r\n"), "want 3-element: {s}");
    // Cursor must be > 2-0 (next-after-last claimed)
    assert!(s.contains("2-1") || s.contains("3-0"), "cursor advanced: {s}");
    // Two entries claimed
    assert!(s.contains("1-0") && s.contains("2-0"));
}

// ───────────── XGROUP CREATECONSUMER / DELCONSUMER ─────────────

#[test]
fn xgroup_createconsumer_then_delconsumer() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    setup_group(&mut c);
    c.write_all(&req(&[
        b"XGROUP", b"CREATECONSUMER", b"s", b"g1", b"c1",
    ]))
    .unwrap();
    assert_eq!(read_reply(&mut c), b":1\r\n");
    c.write_all(&req(&[
        b"XGROUP", b"CREATECONSUMER", b"s", b"g1", b"c1",
    ]))
    .unwrap();
    assert_eq!(read_reply(&mut c), b":0\r\n");
    c.write_all(&req(&[
        b"XREADGROUP", b"GROUP", b"g1", b"c1", b"STREAMS", b"s", b">",
    ]))
    .unwrap();
    let _ = read_reply(&mut c);
    c.write_all(&req(&[
        b"XGROUP", b"DELCONSUMER", b"s", b"g1", b"c1",
    ]))
    .unwrap();
    assert_eq!(read_reply(&mut c), b":3\r\n");
}
