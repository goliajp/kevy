//! Every write path that can touch an indexed row must leave the index
//! agreeing with the keyspace — checked after each verb, by name.
//!
//! Two bugs on the 4.1.1 surface were the same omission on two paths: a
//! key deleted by a multi-key verb left its index entry behind, and a
//! row that arrived by scope migration never entered the index at all.
//! Each got a regression test for its own path, which is right and is
//! also how the next one gets missed — the paths were never enumerated.
//!
//! So this walks the verbs that can create, change, rename or remove an
//! indexed hash row, and after each one asserts `IDX.VERIFY` reports
//! zero in **both** directions: `drift` (an entry whose row is gone or
//! no longer derives that value) and `missing` (a row that derives a
//! value and has no entry). `missing` is the direction that was blind
//! until this branch, and it is the one a forgotten write path shows up
//! in.
//!
//! The verdict names the verb, so a failure says which path stopped
//! maintaining the index rather than that something, somewhere, drifted.
//!
//! A refused verb fails the test rather than being skipped: a case that
//! quietly does nothing is worse than no case. That is how `COPY` came
//! out of this list — kevy has no such command, so the case was testing
//! its own typo.

use std::io::{Read, Write};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

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

fn cmd(s: &mut std::net::TcpStream, parts: &[&[u8]]) -> Vec<u8> {
    s.write_all(&req(parts)).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(40));
    let mut buf = [0u8; 65536];
    let n = s.read(&mut buf).unwrap();
    buf[..n].to_vec()
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
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let dir = std::env::temp_dir().join(format!(
            "kevy-idxcov-{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let dir_thread = dir.clone();
        let handle = std::thread::spawn(move || {
            let rt = kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(4))
                .bind([127, 0, 0, 1], port)
                .shards(4)
                .with_data_dir(dir_thread);
            rt.run(stop_thread).unwrap();
        });
        for _ in 0..400 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
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

/// Pull one `field value` pair out of a VERIFY reply.
fn counter(reply: &[u8], field: &str) -> u64 {
    let text = String::from_utf8_lossy(reply);
    let mut items = text.split("\r\n").filter(|s| !s.is_empty() && !s.starts_with(['*', '$', ':']));
    while let Some(k) = items.next() {
        if k == field {
            return items.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        }
    }
    // A `:N` integer reply carries the number without a `$` prefix, so
    // the filter above drops it; fall back to a positional scan.
    let raw: Vec<&str> = text.split("\r\n").filter(|s| !s.is_empty()).collect();
    for (i, k) in raw.iter().enumerate() {
        if *k == field {
            return raw.get(i + 1).map_or(0, |v| v.trim_start_matches(':').parse().unwrap_or(0));
        }
    }
    0
}

/// Verify, and say which verb we had just run if it is not clean.
fn assert_clean(c: &mut std::net::TcpStream, after: &str) {
    // The index is maintained on the write path, but VERIFY walks it;
    // give a tick to land before reading, and retry rather than
    // asserting on the first sample.
    for attempt in 0..40 {
        let r = cmd(c, &[b"IDX.VERIFY", b"u_age"]);
        if r.starts_with(b"-INDEXBUILDING") {
            std::thread::sleep(std::time::Duration::from_millis(50));
            continue;
        }
        let (drift, missing) = (counter(&r, "drift"), counter(&r, "missing"));
        if drift == 0 && missing == 0 {
            return;
        }
        if attempt == 39 {
            panic!(
                "index disagrees with the keyspace after {after}: \
                 drift {drift}, missing {missing}\n{}",
                String::from_utf8_lossy(&r)
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn ok(c: &mut std::net::TcpStream, parts: &[&[u8]], what: &str) -> Vec<u8> {
    let r = cmd(c, parts);
    assert!(
        !r.starts_with(b"-"),
        "{what} was refused, so this case tests nothing: {}",
        String::from_utf8_lossy(&r)
    );
    r
}

#[test]
fn every_write_path_that_touches_a_row_keeps_the_index_honest() {
    let srv = Server::start();
    let mut c = srv.connect();

    ok(&mut c, &[b"IDX.CREATE", b"u_age", b"ON", b"PREFIX", b"u:", b"FIELD", b"age", b"TYPE", b"i64", b"KIND", b"range"], "IDX.CREATE");

    // Seed enough rows to span shards; the index is global, the rows
    // are not.
    for i in 1..=12 {
        let key = format!("u:{i}");
        let age = format!("{}", 20 + i);
        ok(&mut c, &[b"HSET", key.as_bytes(), b"age", age.as_bytes()], "HSET seed");
    }
    assert_clean(&mut c, "the seed writes");

    // Each entry: a verb, and the argv that exercises it against an
    // indexed row. A refusal fails the test rather than being skipped —
    // a case that quietly does nothing is worse than no case.
    ok(&mut c, &[b"HSET", b"u:1", b"age", b"31"], "HSET overwrite");
    assert_clean(&mut c, "HSET (overwrite the indexed field)");

    ok(&mut c, &[b"HSETNX", b"u:13", b"age", b"40"], "HSETNX");
    assert_clean(&mut c, "HSETNX (new row)");

    ok(&mut c, &[b"HINCRBY", b"u:2", b"age", b"5"], "HINCRBY");
    assert_clean(&mut c, "HINCRBY (indexed field moves)");

    ok(&mut c, &[b"HDEL", b"u:3", b"age"], "HDEL");
    assert_clean(&mut c, "HDEL (the indexed field goes; the row no longer derives)");

    ok(&mut c, &[b"DEL", b"u:4"], "DEL");
    assert_clean(&mut c, "DEL (single key)");

    // The one that was broken: a key removed by a *multi-key* verb.
    ok(&mut c, &[b"DEL", b"u:5", b"u:6"], "multi-key DEL");
    assert_clean(&mut c, "DEL (multi-key)");

    ok(&mut c, &[b"UNLINK", b"u:7", b"u:8"], "multi-key UNLINK");
    assert_clean(&mut c, "UNLINK (multi-key)");

    ok(&mut c, &[b"RENAME", b"u:9", b"u:900"], "RENAME");
    assert_clean(&mut c, "RENAME (row moves inside the prefix)");

    ok(&mut c, &[b"RENAME", b"u:10", b"other:10"], "RENAME out of prefix");
    assert_clean(&mut c, "RENAME (row leaves the indexed prefix)");

    // A type change removes the row: a string is not a hash, so it
    // derives nothing and must leave the index.
    ok(&mut c, &[b"SET", b"u:12", b"not-a-hash"], "SET over a hash");
    assert_clean(&mut c, "SET over an indexed hash (type change)");

    // Expiry is a write path with no client behind it.
    ok(&mut c, &[b"PEXPIRE", b"u:11", b"50"], "PEXPIRE");
    std::thread::sleep(std::time::Duration::from_millis(400));
    assert_clean(&mut c, "PEXPIRE (the row expires with no command to see it)");

    // MSET writes strings, so it cannot create an indexed row — but it
    // can overwrite one, which is the multi-key type change.
    ok(&mut c, &[b"MSET", b"u:1", b"gone", b"u:2", b"gone"], "MSET over hashes");
    assert_clean(&mut c, "MSET (multi-key type change over indexed rows)");
}
