//! Global BM25 across shards (step 4b-server): a MATCH hit's rank must
//! not depend on which shard it landed on. The two-pass fan-out (pass 1
//! sums each shard's corpus counters, pass 2 scores against the global
//! stats) makes an N-shard server produce the SAME ranking + scores as a
//! 1-shard server over identical data. Shard-LOCAL BM25 would diverge —
//! each shard's n_docs/avgdl/df differ — so this test fails without 4b.

use std::io::{Read, Write};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

mod common;

static START_GATE: Mutex<()> = Mutex::new(());

fn req(parts: &[&[u8]]) -> Vec<u8> {
    let mut v = format!("*{}\r\n", parts.len()).into_bytes();
    for p in parts {
        v.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
        v.extend_from_slice(p);
        v.extend_from_slice(b"\r\n");
    }
    v
}

/// Send one command and read back exactly one COMPLETE reply.
///
/// This was "sleep 30 ms, then `read()` once", which holds while every
/// reply arrives in one segment and desynchronises the connection the
/// first time one does not: the tail stays in the socket and every later
/// reply is read a frame late. `table_e2e`'s copy failed exactly that way
/// under load — `EXEC` answers `*1\r\n:0\r\n`, an assertion checked only
/// the `*1\r\n` prefix, and `:0\r\n` came back on the front of the next
/// reply. The frame now says when it is complete, and anything left over
/// is a failure rather than a shift.
fn cmd(s: &mut std::net::TcpStream, parts: &[&[u8]]) -> Vec<u8> {
    s.write_all(&req(parts)).unwrap();
    let mut buf = Vec::new();
    loop {
        if let Some(n) = common::reply_len(&buf) {
            assert_eq!(
                n,
                buf.len(),
                "{} extra byte(s) after the reply — the connection is a frame ahead: {:?}",
                buf.len() - n,
                String::from_utf8_lossy(&buf[n..]).chars().take(60).collect::<String>()
            );
            return buf;
        }
        let mut chunk = [0u8; 65536];
        let got = s.read(&mut chunk).unwrap();
        assert!(got > 0, "server closed mid-reply (have {} bytes)", buf.len());
        buf.extend_from_slice(&chunk[..got]);
    }
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
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let dir = std::env::temp_dir().join(format!(
            "kevy-gbm25-{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
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
        kevy_testnet::assert_listening(port, "the server under test");
        Self { port, dir, stop, handle: Some(handle) }
    }

    fn connect(&self) -> std::net::TcpStream {
        let s = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(8))).unwrap();
        s
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = std::net::TcpStream::connect(("127.0.0.1", self.port));
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn query_ready(c: &mut std::net::TcpStream, parts: &[&[u8]]) -> Vec<u8> {
    for _ in 0..100 {
        let r = cmd(c, parts);
        if !r.starts_with(b"-INDEXBUILDING") {
            return r;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("index never became ready");
}

/// A corpus with skewed doc lengths and term frequencies so BM25's
/// length normalization AND idf both bite — the two knobs that shard-local
/// scoring gets wrong. Keys are deterministic so both servers hash them to
/// the same (different-count) shard layout.
const DOCS: &[(&str, &str)] = &[
    ("doc:01", "rust rust rust systems programming language"),
    ("doc:02", "rust is a systems language with memory safety and zero cost abstractions across the whole stack"),
    ("doc:03", "python scripting language dynamic typing"),
    ("doc:04", "rust text search engine full text indexing"),
    ("doc:05", "search search relevance ranking bm25 scoring"),
    ("doc:06", "the quick brown fox jumps over the lazy dog while a rust crab watches"),
    ("doc:07", "memory safety without garbage collection is the rust promise"),
    ("doc:08", "systems programming in rust and c and cpp compared at length here today"),
    ("doc:09", "search text rust"),
    ("doc:10", "a distributed key value store written in pure rust with text search"),
    ("doc:11", "database indexing structures btree inverted index posting lists"),
    ("doc:12", "rust"),
    ("doc:13", "full text search over documents with ranked bm25 results and stemming later"),
    ("doc:14", "concurrency in rust fearless threads channels and async await runtime"),
    ("doc:15", "the language wars rust versus go versus zig for systems work continue"),
    ("doc:16", "text text text search search rust"),
    ("doc:17", "ranking documents by relevance is what a search engine does at its core"),
    ("doc:18", "rust rust systems systems language language search text"),
    ("doc:19", "an unrelated document about cooking pasta and sauce with basil"),
    ("doc:20", "another filler doc mentioning rust exactly once among plenty of other words here"),
];

/// Parse a RESP array-of-arrays MATCH reply into `[(key, score_bytes)]`
/// (first two bulk fields of each row).
fn parse_ranked(reply: &[u8]) -> Vec<(String, String)> {
    let s = String::from_utf8_lossy(reply);
    let mut lines = s.split("\r\n");
    let head = lines.next().unwrap_or("");
    assert!(head.starts_with('*'), "expected array reply, got: {s:?}");
    let nrows: i64 = head[1..].parse().unwrap();
    let mut rows = Vec::new();
    for _ in 0..nrows {
        // row header "*<k>"
        let rh = lines.next().unwrap();
        assert!(rh.starts_with('*'));
        // key: "$len" then bytes
        lines.next(); // $len
        let key = lines.next().unwrap().to_string();
        lines.next(); // $len
        let score = lines.next().unwrap().to_string();
        // skip any remaining field pairs in this row
        let k: i64 = rh[1..].parse().unwrap();
        for _ in 0..(k - 2) {
            lines.next();
            lines.next();
        }
        rows.push((key, score));
    }
    rows
}

fn seed_and_query(nshards: usize) -> Vec<(String, String)> {
    let srv = Server::start(nshards);
    let mut c = srv.connect();
    for (k, body) in DOCS {
        cmd(&mut c, &[b"HSET", k.as_bytes(), b"body", body.as_bytes()]);
    }
    cmd(
        &mut c,
        &[b"IDX.CREATE", b"d_body", b"ON", b"PREFIX", b"doc:", b"FIELD", b"body", b"TYPE", b"str", b"KIND", b"text"],
    );
    let r = query_ready(&mut c, &[b"IDX.QUERY", b"d_body", b"MATCH", b"rust text search", b"LIMIT", b"20"]);
    parse_ranked(&r)
}

/// The core invariance: 1 shard vs 8 shards → identical ranked keys AND
/// identical scores (to the 4-decimal wire precision). Shard-local BM25
/// would give each shard its own n_docs/avgdl/df, so the 8-shard scores
/// would differ — this equality is exactly what global BM25 buys.
#[test]
fn match_ranking_is_shard_invariant() {
    let one = seed_and_query(1);
    let eight = seed_and_query(8);
    assert_eq!(one, eight, "1-shard vs 8-shard MATCH ranking+scores diverged");
    // Sanity: the query actually matched a non-trivial slice of the corpus
    // (so the equality above isn't vacuously over an empty list).
    assert!(one.len() >= 10, "expected many hits, got {}", one.len());
    // BM25 length normalization: the terse, dense doc:16 ("text text text
    // search search rust") should outrank the long doc:02 that mentions
    // "rust" once in a 17-word body.
    let pos = |k: &str| one.iter().position(|(key, _)| key == k);
    assert!(pos("doc:16") < pos("doc:02"), "length normalization off: {one:?}");
}

/// FIELDS hydration still rides the pass-2 chunk unchanged.
#[test]
fn match_with_fields_hydrates_across_shards() {
    let srv = Server::start(8);
    let mut c = srv.connect();
    for (k, body) in DOCS {
        cmd(&mut c, &[b"HSET", k.as_bytes(), b"body", body.as_bytes()]);
    }
    cmd(
        &mut c,
        &[b"IDX.CREATE", b"d_body", b"ON", b"PREFIX", b"doc:", b"FIELD", b"body", b"TYPE", b"str", b"KIND", b"text"],
    );
    let r = query_ready(
        &mut c,
        &[b"IDX.QUERY", b"d_body", b"MATCH", b"rust", b"LIMIT", b"3", b"FIELDS", b"body"],
    );
    let s = String::from_utf8_lossy(&r);
    assert!(s.contains("body"), "FIELDS name absent from hydrated reply: {s:?}");
    assert!(s.contains("rust"), "hydrated body value absent: {s:?}");
}
