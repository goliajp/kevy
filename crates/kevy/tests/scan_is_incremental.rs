//! v4 detection: `SCAN` must be a REAL cursor iterator — bounded work
//! per call (COUNT buckets, not the whole keyspace), non-zero cursors
//! that page through the sharded keyspace, exact set coverage over a
//! full sweep, MATCH filtering, and the dictScan rehash guarantee
//! (keys present throughout a sweep are returned at least once even
//! when the tables grow mid-sweep). The old code swept EVERY shard's
//! ENTIRE keyspace per call and always replied cursor 0 — tests 1/2/4
//! fail against it.

use std::io::{BufRead, BufReader, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Serialize server startup across this binary's parallel tests (see
/// `tests/sharded.rs` for the SO_REUSEPORT race rationale).
static START_GATE: Mutex<()> = Mutex::new(());

use kevy_testnet::free_port;

fn req(parts: &[&[u8]]) -> Vec<u8> {
    let mut v = format!("*{}\r\n", parts.len()).into_bytes();
    for p in parts {
        v.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
        v.extend_from_slice(p);
        v.extend_from_slice(b"\r\n");
    }
    v
}

struct Server {
    port: u16,
    dir: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start(nshards: usize) -> Server {
        let _gate = START_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let port = free_port();
        let dir = std::env::temp_dir().join(format!(
            "kevy-scan-{}",
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
            let rt = kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(nshards))
                .bind([127, 0, 0, 1], port)
                .shards(nshards)
                .with_data_dir(dir_thread);
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
        Server { port, dir, stop, handle: Some(handle) }
    }

    fn connect(&self) -> BufReader<std::net::TcpStream> {
        let s = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(30))).unwrap();
        BufReader::new(s)
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

/// Read one CRLF-terminated header line (without the CRLF).
fn read_line(c: &mut BufReader<std::net::TcpStream>) -> String {
    let mut line = String::new();
    c.read_line(&mut line).unwrap();
    assert!(line.ends_with("\r\n"), "unterminated RESP line: {line:?}");
    line.truncate(line.len() - 2);
    line
}

/// Read a bulk string given its already-consumed `$<len>` header value.
fn read_bulk_body(c: &mut BufReader<std::net::TcpStream>, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len + 2];
    c.read_exact(&mut buf).unwrap();
    buf.truncate(len);
    buf
}

/// Issue `SCAN cursor [args…]` and parse the `[cursor, [keys]]` reply.
/// Returns `(next_cursor, keys, reply_bytes)`.
fn scan(
    c: &mut BufReader<std::net::TcpStream>,
    cursor: u64,
    extra: &[&[u8]],
) -> (u64, Vec<Vec<u8>>, usize) {
    let cur = cursor.to_string();
    let mut parts: Vec<&[u8]> = vec![b"SCAN", cur.as_bytes()];
    parts.extend_from_slice(extra);
    c.get_mut().write_all(&req(&parts)).unwrap();

    let hdr = read_line(c);
    assert_eq!(hdr, "*2", "SCAN reply must be a 2-element array, got {hdr}");
    let mut reply_bytes = hdr.len() + 2;

    let chdr = read_line(c);
    let clen: usize = chdr.strip_prefix('$').unwrap().parse().unwrap();
    let cbody = read_bulk_body(c, clen);
    let next: u64 = String::from_utf8(cbody).unwrap().parse().unwrap();
    reply_bytes += chdr.len() + 2 + clen + 2;

    let ahdr = read_line(c);
    let n: usize = ahdr.strip_prefix('*').unwrap().parse().unwrap();
    reply_bytes += ahdr.len() + 2;
    let mut keys = Vec::with_capacity(n);
    for _ in 0..n {
        let khdr = read_line(c);
        let klen: usize = khdr.strip_prefix('$').unwrap().parse().unwrap();
        keys.push(read_bulk_body(c, klen));
        reply_bytes += khdr.len() + 2 + klen + 2;
    }
    (next, keys, reply_bytes)
}

/// Pipeline `SET <key> x` for every key; read all the +OK replies.
fn load_keys(c: &mut BufReader<std::net::TcpStream>, keys: impl Iterator<Item = String>) {
    let mut batch = Vec::new();
    let mut count = 0usize;
    for k in keys {
        batch.extend_from_slice(&req(&[b"SET", k.as_bytes(), b"x"]));
        count += 1;
        if batch.len() >= 1 << 20 {
            c.get_mut().write_all(&batch).unwrap();
            batch.clear();
            for _ in 0..count {
                assert_eq!(read_line(c), "+OK");
            }
            count = 0;
        }
    }
    if count > 0 {
        c.get_mut().write_all(&batch).unwrap();
        for _ in 0..count {
            assert_eq!(read_line(c), "+OK");
        }
    }
}

/// Follow cursors from 0 until the server replies 0 again; returns every
/// key seen (dupes preserved) plus the call count.
fn full_sweep(
    c: &mut BufReader<std::net::TcpStream>,
    extra: &[&[u8]],
    max_calls: usize,
) -> (Vec<Vec<u8>>, usize) {
    let mut all = Vec::new();
    let mut cursor = 0u64;
    let mut calls = 0usize;
    loop {
        let (next, keys, _) = scan(c, cursor, extra);
        all.extend(keys);
        calls += 1;
        assert!(calls <= max_calls, "cursor loop did not terminate in {max_calls} calls");
        if next == 0 {
            return (all, calls);
        }
        cursor = next;
    }
}

/// Test 1 — bounded first page: on a 50k-key server, `SCAN 0 COUNT 10`
/// must return a NON-zero cursor and a small reply (the old code
/// returned the whole 50k-key keyspace, megabytes, with cursor 0).
/// Test 2 (same keyspace, spares a second 50k load) — a full COUNT 100
/// cursor loop collects EXACTLY the 50k distinct keys (set coverage;
/// duplicates are legal cursor protocol).
#[test]
fn scan_pages_are_bounded_and_cover_exactly() {
    let srv = Server::start(8);
    let mut c = srv.connect();
    load_keys(&mut c, (0..50_000u32).map(|i| format!("k:{i}")));

    // Test 1: first page is bounded.
    let (next, keys, reply_bytes) = scan(&mut c, 0, &[b"COUNT", b"10"]);
    assert_ne!(next, 0, "SCAN 0 COUNT 10 on 50k keys must page, not finish");
    assert!(
        reply_bytes < 64 * 1024,
        "first page is {reply_bytes} bytes — the old whole-keyspace sweep"
    );
    assert!(
        keys.len() < 1_000,
        "first page returned {} keys for COUNT 10",
        keys.len()
    );

    // Test 2: full loop = exact set coverage.
    let (all, calls) = full_sweep(&mut c, &[b"COUNT", b"100"], 50_000);
    assert!(calls > 10, "50k keys at COUNT 100 finished in {calls} calls — not incremental");
    let distinct: std::collections::HashSet<Vec<u8>> = all.into_iter().collect();
    assert_eq!(distinct.len(), 50_000, "sweep missed or invented keys");
    for i in 0..50_000u32 {
        assert!(
            distinct.contains(format!("k:{i}").as_bytes()),
            "key k:{i} never returned"
        );
    }
}

/// Test 3 — MATCH over a mixed keyspace returns exactly the matching set.
#[test]
fn scan_match_returns_exactly_the_matching_set() {
    let srv = Server::start(8);
    let mut c = srv.connect();
    load_keys(&mut c, (0..3_000u32).map(|i| format!("user:{i}")));
    load_keys(&mut c, (0..3_000u32).map(|i| format!("order:{i}")));

    let (all, _) = full_sweep(&mut c, &[b"MATCH", b"user:*", b"COUNT", b"100"], 50_000);
    let distinct: std::collections::HashSet<Vec<u8>> = all.into_iter().collect();
    assert_eq!(distinct.len(), 3_000);
    for k in &distinct {
        assert!(k.starts_with(b"user:"), "non-matching key {:?}", String::from_utf8_lossy(k));
    }
}

/// Test 4 — rehash tolerance: start a sweep, grow the tables mid-sweep
/// by inserting 10k MORE keys, finish the sweep — every ORIGINAL key
/// still appears at least once (Redis dictScan's guarantee).
#[test]
fn scan_survives_rehash_mid_sweep() {
    let srv = Server::start(8);
    let mut c = srv.connect();
    load_keys(&mut c, (0..8_000u32).map(|i| format!("orig:{i}")));

    let mut seen: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
    let mut cursor = 0u64;
    // A few paging calls first…
    for _ in 0..5 {
        let (next, keys, _) = scan(&mut c, cursor, &[b"COUNT", b"100"]);
        seen.extend(keys);
        assert_ne!(next, 0, "sweep finished before the grow could happen");
        cursor = next;
    }
    // …then grow every shard's table (8k → 18k keys ≈ a doubling or two)…
    load_keys(&mut c, (0..10_000u32).map(|i| format!("grow:{i}")));
    // …and finish the sweep.
    let mut calls = 0usize;
    while cursor != 0 {
        let (next, keys, _) = scan(&mut c, cursor, &[b"COUNT", b"100"]);
        seen.extend(keys);
        cursor = next;
        calls += 1;
        assert!(calls <= 50_000, "cursor loop did not terminate");
    }
    for i in 0..8_000u32 {
        assert!(
            seen.contains(format!("orig:{i}").as_bytes()),
            "original key orig:{i} skipped across the rehash"
        );
    }
}

/// TYPE option — previously accepted-and-ignored, now a real filter:
/// only keys holding the requested value type come back (unknown type
/// names match nothing).
#[test]
fn scan_type_filters_by_value_type() {
    let srv = Server::start(8);
    let mut c = srv.connect();
    load_keys(&mut c, (0..500u32).map(|i| format!("str:{i}")));
    let mut batch = Vec::new();
    for i in 0..500u32 {
        batch.extend_from_slice(&req(&[b"LPUSH", format!("list:{i}").as_bytes(), b"x"]));
    }
    c.get_mut().write_all(&batch).unwrap();
    for _ in 0..500 {
        assert_eq!(read_line(&mut c), ":1");
    }

    let (lists, _) = full_sweep(&mut c, &[b"TYPE", b"list", b"COUNT", b"100"], 50_000);
    let distinct: std::collections::HashSet<Vec<u8>> = lists.into_iter().collect();
    assert_eq!(distinct.len(), 500);
    for k in &distinct {
        assert!(k.starts_with(b"list:"), "TYPE list returned {:?}", String::from_utf8_lossy(k));
    }

    let (none, _) = full_sweep(&mut c, &[b"TYPE", b"nosuchtype", b"COUNT", b"100"], 50_000);
    assert!(none.is_empty(), "unknown TYPE must match nothing");
}

/// Test 5 — empty db: `SCAN 0` replies cursor 0 with an empty array in
/// a single call (the shard chain must run to completion server-side).
#[test]
fn scan_on_empty_db_finishes_in_one_call() {
    let srv = Server::start(8);
    let mut c = srv.connect();
    let (next, keys, _) = scan(&mut c, 0, &[]);
    assert_eq!(next, 0);
    assert!(keys.is_empty());

    // Error paths (Redis wording), on the same server.
    let expect_err = |c: &mut BufReader<std::net::TcpStream>, parts: &[&[u8]], want: &str| {
        c.get_mut().write_all(&req(parts)).unwrap();
        let line = read_line(c);
        assert_eq!(line, format!("-{want}"), "for {parts:?}");
    };
    expect_err(&mut c, &[b"SCAN", b"notanumber"], "ERR invalid cursor");
    expect_err(&mut c, &[b"SCAN", b"0", b"COUNT", b"0"], "ERR syntax error");
    expect_err(
        &mut c,
        &[b"SCAN", b"0", b"COUNT", b"abc"],
        "ERR value is not an integer or out of range",
    );
    expect_err(&mut c, &[b"SCAN", b"0", b"BADOPT", b"x"], "ERR syntax error");
    expect_err(&mut c, &[b"SCAN", b"0", b"MATCH"], "ERR syntax error");
}
