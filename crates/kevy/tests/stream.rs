//! `XADD` / `XLEN` / `XRANGE` / `XREVRANGE` / `XDEL` / `XTRIM` /
//! `XREAD` — sprint A of v2-7 Streams.

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
        other => panic!("unknown reply prefix {other:?}"),
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
        let _gate = START_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let port = free_port();
        let dir = std::env::temp_dir().join(format!(
            "kevy-stream-{}",
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

// ───────────── XADD ─────────────

#[test]
fn xadd_explicit_id_returns_id_bulk() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    c.write_all(&req(&[b"XADD", b"s", b"1-0", b"field", b"value"]))
        .unwrap();
    assert_eq!(read_reply(&mut c), b"$3\r\n1-0\r\n");
}

#[test]
fn xadd_auto_id_monotonic() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    c.write_all(&req(&[b"XADD", b"s", b"*", b"f", b"v1"])).unwrap();
    let r1 = read_reply(&mut c);
    c.write_all(&req(&[b"XADD", b"s", b"*", b"f", b"v2"])).unwrap();
    let r2 = read_reply(&mut c);
    let id1 = String::from_utf8(r1[5..r1.len() - 2].to_vec()).unwrap();
    let id2 = String::from_utf8(r2[5..r2.len() - 2].to_vec()).unwrap();
    assert_ne!(id1, id2, "auto IDs must differ: {id1} vs {id2}");
    // Compare lexicographically with zero-padding via parsing.
    let parse = |s: &str| -> (u64, u64) {
        let (a, b) = s.split_once('-').unwrap();
        (a.parse().unwrap(), b.parse().unwrap())
    };
    assert!(parse(&id2) > parse(&id1), "auto IDs must be increasing");
}

#[test]
fn xadd_explicit_must_be_strictly_increasing() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    c.write_all(&req(&[b"XADD", b"s", b"5-0", b"f", b"v1"])).unwrap();
    assert_eq!(read_reply(&mut c), b"$3\r\n5-0\r\n");
    c.write_all(&req(&[b"XADD", b"s", b"5-0", b"f", b"v2"])).unwrap();
    let r = read_reply(&mut c);
    assert!(r.starts_with(b"-ERR"), "expected error: {:?}", String::from_utf8_lossy(&r));
    assert!(
        String::from_utf8_lossy(&r).contains("ID specified in XADD"),
        "got: {:?}",
        String::from_utf8_lossy(&r),
    );
}

#[test]
fn xadd_partial_auto_seq() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    c.write_all(&req(&[b"XADD", b"s", b"7-*", b"f", b"v1"])).unwrap();
    assert_eq!(read_reply(&mut c), b"$3\r\n7-0\r\n");
    c.write_all(&req(&[b"XADD", b"s", b"7-*", b"f", b"v2"])).unwrap();
    assert_eq!(read_reply(&mut c), b"$3\r\n7-1\r\n");
}

#[test]
fn xadd_nomkstream_returns_nil_when_missing() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    c.write_all(&req(&[b"XADD", b"s", b"NOMKSTREAM", b"*", b"f", b"v"]))
        .unwrap();
    assert_eq!(read_reply(&mut c), b"$-1\r\n");
    c.write_all(&req(&[b"EXISTS", b"s"])).unwrap();
    assert_eq!(read_reply(&mut c), b":0\r\n");
}

// ───────────── XLEN ─────────────

#[test]
fn xlen_after_inserts() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    for i in 1..=3 {
        let id = format!("{i}-0");
        c.write_all(&req(&[b"XADD", b"s", id.as_bytes(), b"f", b"v"]))
            .unwrap();
        let _ = read_reply(&mut c);
    }
    c.write_all(&req(&[b"XLEN", b"s"])).unwrap();
    assert_eq!(read_reply(&mut c), b":3\r\n");
}

#[test]
fn xlen_missing_key_is_zero() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    c.write_all(&req(&[b"XLEN", b"none"])).unwrap();
    assert_eq!(read_reply(&mut c), b":0\r\n");
}

// ───────────── XRANGE / XREVRANGE ─────────────

fn seed_three(c: &mut std::net::TcpStream) {
    for (id, v) in [("1-0", b"a" as &[u8]), ("2-0", b"b"), ("3-0", b"c")] {
        c.write_all(&req(&[b"XADD", b"s", id.as_bytes(), b"f", v])).unwrap();
        let _ = read_reply(c);
    }
}

#[test]
fn xrange_inclusive_and_dash_plus() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    seed_three(&mut c);
    c.write_all(&req(&[b"XRANGE", b"s", b"-", b"+"])).unwrap();
    let r = read_reply(&mut c);
    let s = String::from_utf8_lossy(&r);
    assert!(s.starts_with("*3\r\n"), "expected 3 entries: {s}");
    assert!(s.contains("1-0") && s.contains("2-0") && s.contains("3-0"));
    // Inner shape: each entry = *2 (id + *2 fields)
    assert!(s.contains("*2\r\n$3\r\n1-0\r\n*2\r\n$1\r\nf\r\n$1\r\na\r\n"));
}

#[test]
fn xrange_count_truncates() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    seed_three(&mut c);
    c.write_all(&req(&[b"XRANGE", b"s", b"-", b"+", b"COUNT", b"2"])).unwrap();
    let r = read_reply(&mut c);
    assert!(String::from_utf8_lossy(&r).starts_with("*2\r\n"));
}

#[test]
fn xrevrange_descending() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    seed_three(&mut c);
    c.write_all(&req(&[b"XREVRANGE", b"s", b"+", b"-"])).unwrap();
    let r = read_reply(&mut c);
    let s = String::from_utf8_lossy(&r);
    // First entry id must be 3-0
    let p3 = s.find("3-0").unwrap();
    let p1 = s.find("1-0").unwrap();
    assert!(p3 < p1, "expected descending: {s}");
}

#[test]
fn xrange_partial_id_treats_seq_as_zero_or_max() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    c.write_all(&req(&[b"XADD", b"s", b"5-1", b"f", b"a"])).unwrap();
    let _ = read_reply(&mut c);
    c.write_all(&req(&[b"XADD", b"s", b"5-2", b"f", b"b"])).unwrap();
    let _ = read_reply(&mut c);
    // "5" as start = 5-0; "5" as end = 5-MAX → both 5-1 and 5-2.
    c.write_all(&req(&[b"XRANGE", b"s", b"5", b"5"])).unwrap();
    let r = read_reply(&mut c);
    let s = String::from_utf8_lossy(&r);
    assert!(s.starts_with("*2\r\n"), "expected 2 entries for 5..5: {s}");
}

// ───────────── XDEL / XTRIM ─────────────

#[test]
fn xdel_removes_known_ids() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    seed_three(&mut c);
    c.write_all(&req(&[b"XDEL", b"s", b"2-0", b"99-0"])).unwrap();
    assert_eq!(read_reply(&mut c), b":1\r\n");
    c.write_all(&req(&[b"XLEN", b"s"])).unwrap();
    assert_eq!(read_reply(&mut c), b":2\r\n");
}

#[test]
fn xtrim_maxlen_drops_oldest() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    seed_three(&mut c);
    c.write_all(&req(&[b"XTRIM", b"s", b"MAXLEN", b"2"])).unwrap();
    assert_eq!(read_reply(&mut c), b":1\r\n");
    c.write_all(&req(&[b"XRANGE", b"s", b"-", b"+"])).unwrap();
    let r = read_reply(&mut c);
    let s = String::from_utf8_lossy(&r);
    assert!(s.contains("2-0") && s.contains("3-0"));
    assert!(!s.contains("1-0"), "oldest must be gone: {s}");
}

#[test]
fn xtrim_minid_drops_below_threshold() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    seed_three(&mut c);
    c.write_all(&req(&[b"XTRIM", b"s", b"MINID", b"2-0"])).unwrap();
    assert_eq!(read_reply(&mut c), b":1\r\n");
}

#[test]
fn xadd_with_maxlen_trims_inline() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    seed_three(&mut c);
    c.write_all(&req(&[
        b"XADD", b"s", b"MAXLEN", b"2", b"4-0", b"f", b"d",
    ]))
    .unwrap();
    assert_eq!(read_reply(&mut c), b"$3\r\n4-0\r\n");
    c.write_all(&req(&[b"XLEN", b"s"])).unwrap();
    assert_eq!(read_reply(&mut c), b":2\r\n");
}

// ───────────── XREAD ─────────────

#[test]
fn xread_returns_entries_after_last_seen() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    seed_three(&mut c);
    c.write_all(&req(&[b"XREAD", b"STREAMS", b"s", b"0"])).unwrap();
    let r = read_reply(&mut c);
    let s = String::from_utf8_lossy(&r);
    assert!(s.starts_with("*1\r\n"), "expected 1 stream: {s}");
    assert!(s.contains("1-0") && s.contains("2-0") && s.contains("3-0"));
}

#[test]
fn xread_after_specific_id_skips_earlier() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    seed_three(&mut c);
    c.write_all(&req(&[b"XREAD", b"STREAMS", b"s", b"2-0"])).unwrap();
    let r = read_reply(&mut c);
    let s = String::from_utf8_lossy(&r);
    assert!(s.contains("3-0"));
    assert!(!s.contains("\r\n1-0\r\n"));
    assert!(!s.contains("\r\n2-0\r\n"));
}

#[test]
fn xread_dollar_means_only_new_entries() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    seed_three(&mut c);
    c.write_all(&req(&[b"XREAD", b"STREAMS", b"s", b"$"])).unwrap();
    // $ resolves to last_id BEFORE any new XADD; without BLOCK in
    // sprint A, no new entries can appear → null array.
    assert_eq!(read_reply(&mut c), b"*-1\r\n");
}

#[test]
fn xread_count_truncates() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    seed_three(&mut c);
    c.write_all(&req(&[b"XREAD", b"COUNT", b"1", b"STREAMS", b"s", b"0"]))
        .unwrap();
    let r = read_reply(&mut c);
    let s = String::from_utf8_lossy(&r);
    // *1 (one stream) → *2 → key + entries
    assert!(s.starts_with("*1\r\n"));
    // entries array length should be 1
    assert!(s.contains("\r\n*1\r\n*2\r\n$3\r\n1-0\r\n"));
}

#[test]
fn xread_no_entries_returns_null_array() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    c.write_all(&req(&[b"XREAD", b"STREAMS", b"none", b"0"])).unwrap();
    assert_eq!(read_reply(&mut c), b"*-1\r\n");
}

// ───────────── TYPE / wrong-type ─────────────

#[test]
fn type_of_stream_key_is_stream() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    c.write_all(&req(&[b"XADD", b"s", b"*", b"f", b"v"])).unwrap();
    let _ = read_reply(&mut c);
    c.write_all(&req(&[b"TYPE", b"s"])).unwrap();
    assert_eq!(read_reply(&mut c), b"+stream\r\n");
}

#[test]
fn xadd_on_wrong_type_errors() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    c.write_all(&req(&[b"SET", b"s", b"plain-string"])).unwrap();
    let _ = read_reply(&mut c);
    c.write_all(&req(&[b"XADD", b"s", b"*", b"f", b"v"])).unwrap();
    let r = read_reply(&mut c);
    assert!(r.starts_with(b"-WRONGTYPE"), "got: {:?}", String::from_utf8_lossy(&r));
}

// ───────────── XSETID (2026-06-11) ─────────────

#[test]
fn xsetid_bumps_clock_and_rejects_rollback() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    c.write_all(&req(&[b"XSETID", b"nope", b"1-0"])).unwrap();
    assert_eq!(
        read_reply(&mut c),
        b"-ERR The XSETID command requires the key to exist.\r\n"
    );
    c.write_all(&req(&[b"XADD", b"s", b"5-1", b"f", b"v"])).unwrap();
    assert_eq!(read_reply(&mut c), b"$3\r\n5-1\r\n");
    c.write_all(&req(&[b"XSETID", b"s", b"4-0"])).unwrap();
    assert_eq!(
        read_reply(&mut c),
        b"-ERR The ID specified in XSETID is smaller than the target stream top item\r\n"
    );
    c.write_all(&req(&[
        b"XSETID", b"s", b"9-0", b"ENTRIESADDED", b"42", b"MAXDELETEDID", b"3-3",
    ]))
    .unwrap();
    assert_eq!(read_reply(&mut c), b"+OK\r\n");
    // The bumped clock now gates XADD.
    c.write_all(&req(&[b"XADD", b"s", b"9-0", b"f", b"v"])).unwrap();
    assert_eq!(
        read_reply(&mut c),
        b"-ERR The ID specified in XADD is equal or smaller than the target stream top item\r\n"
    );
    c.write_all(&req(&[b"XADD", b"s", b"9-1", b"f", b"v"])).unwrap();
    assert_eq!(read_reply(&mut c), b"$3\r\n9-1\r\n");
}
