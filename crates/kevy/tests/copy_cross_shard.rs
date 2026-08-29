//! `COPY src dst [REPLACE]` on a MULTI-SHARD server.
//!
//! COPY names two keys, and on a thread-per-core server two arbitrary
//! keys are usually on different cores. Left to the router's catch-all
//! (`Route::Single(1)` — hash args[1], the source) the copy would be
//! written into the SOURCE's shard, where no later read of the
//! destination looks, while the command replied `:1`. That is the
//! failure `exec_listmove` was written for, and this file exists so
//! COPY cannot repeat it.
//!
//! So: eight shards, and **every test asserts what the DESTINATION
//! holds** — its value, and where a TTL is involved, that the TTL came
//! with it. A test that only read the reply would pass against the
//! broken shape.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

mod common;

static START_GATE: Mutex<()> = Mutex::new(());

/// One server for every test that only reads and writes its own keys.
///
/// The first draft started one per test — eight servers of eight shards
/// each, which is eight ports, and the workspace suite already saturates
/// `free_port`'s window between handing a port out and the server
/// binding it (`kevy-testnet` names that race in its own panic text).
/// The tests below use disjoint key prefixes, so they have no reason to
/// want a keyspace of their own. The restart test keeps its own server
/// because it stops one.
fn shared() -> &'static Server {
    static S: std::sync::OnceLock<Server> = std::sync::OnceLock::new();
    S.get_or_init(Server::start)
}

/// Eight shards: enough that the key pairs below are almost never
/// co-located, which is the condition the cross-shard path needs.
const SHARDS: usize = 8;

struct Server {
    port: u16,
    dir: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start() -> Self {
        let _gate = START_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let port = kevy_testnet::free_port();
        let dir = std::env::temp_dir().join(format!(
            "kevy-copyxshard-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let (st, d) = (stop.clone(), dir.clone());
        let handle = std::thread::spawn(move || {
            kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(SHARDS))
                .bind([127, 0, 0, 1], port)
                .shards(SHARDS)
                .with_data_dir(d)
                .run(st)
                .unwrap();
        });
        kevy_testnet::assert_listening(port, "the cross-shard COPY server");
        Self { port, dir, stop, handle: Some(handle) }
    }

    fn wire(&self) -> common::Wire {
        let sock = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        sock.set_read_timeout(Some(std::time::Duration::from_secs(8))).unwrap();
        common::Wire::new(sock)
    }
}

impl Server {
    /// Stop the server and bring another up on the same data directory.
    /// The only way to ask whether a cross-shard copy was written down
    /// as well as remembered: `op_copy_put` logs through the same
    /// `log_value_placed` RENAME's put uses, and "the same call" is a
    /// claim about the code, not about the file.
    fn restart(self) -> Self {
        let dir = self.dir.clone();
        let _gate = START_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        self.stop.store(true, Ordering::SeqCst);
        let _ = std::net::TcpStream::connect(("127.0.0.1", self.port));
        let mut me = self;
        if let Some(h) = me.handle.take() {
            let _ = h.join();
        }
        std::mem::forget(me); // the Drop below would delete `dir`
        let port = kevy_testnet::free_port();
        let stop = Arc::new(AtomicBool::new(false));
        let (st, d) = (stop.clone(), dir.clone());
        let handle = std::thread::spawn(move || {
            kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(SHARDS))
                .bind([127, 0, 0, 1], port)
                .shards(SHARDS)
                .with_data_dir(d)
                .run(st)
                .unwrap();
        });
        kevy_testnet::assert_listening(port, "the restarted COPY server");
        Server { port, dir, stop, handle: Some(handle) }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = std::net::TcpStream::connect(("127.0.0.1", self.port));
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn s(b: &[u8]) -> String {
    String::from_utf8_lossy(b).replace("\r\n", "\\r\\n")
}

/// Twelve pairs, so at least one lands cross-shard on any hash: the
/// point of the file is that the cross-shard arm runs, not that a
/// particular pair happens to split.
const PAIRS: &[(&str, &str)] = &[
    ("ca", "cb"),
    ("cc", "cd"),
    ("ce", "cf"),
    ("cg", "ch"),
    ("ci", "cj"),
    ("ck", "cl"),
    ("cm", "cn"),
    ("co", "cp"),
    ("cq", "cr"),
    ("cs", "ct"),
    ("cu", "cv"),
    ("cw", "cx"),
];

/// The floor for every test below: they are only about the cross-shard
/// path if the pairs actually split, and "eight shards, so surely they
/// do" is an assumption, not a measurement. `kevy_rt::shard_of_key` is
/// the same function the server routes with, so this asks it.
#[test]
fn the_pairs_this_file_uses_are_really_on_different_shards() {
    let split = PAIRS
        .iter()
        .filter(|(a, b)| {
            kevy_rt::shard_of_key(a.as_bytes(), SHARDS, false)
                != kevy_rt::shard_of_key(b.as_bytes(), SHARDS, false)
        })
        .count();
    println!("cross-shard pairs: {split} of {}", PAIRS.len());
    // The prefixed variants the other tests build ("ttl-ca" and so on)
    // hash differently again, so this is a floor on the bare pairs and
    // the weakest claim that still makes the file mean what it says.
    assert!(
        split >= PAIRS.len() / 2,
        "only {split} of {} pairs are cross-shard — this file would be \
         testing the same-shard path under a cross-shard name",
        PAIRS.len()
    );
}

#[test]
fn every_copy_lands_where_the_destination_is_read_from() {
    let server = shared();
    let mut w = server.wire();

    for (src, dst) in PAIRS {
        assert_eq!(s(&w.call(&["SET", src, "value-of-source"])), "+OK\\r\\n");
        assert_eq!(s(&w.call(&["COPY", src, dst])), ":1\\r\\n", "COPY {src} -> {dst}");
    }
    // Read every destination back — on a fresh connection, so nothing
    // about the write path's own shard can be carrying the answer.
    let mut r = server.wire();
    let mut lost = Vec::new();
    for (src, dst) in PAIRS {
        let got = r.call(&["GET", dst]);
        if got != b"$15\r\nvalue-of-source\r\n" {
            lost.push(format!("{src} -> {dst}: {}", s(&got)));
        }
        // And the source is still there: a copy is not a move.
        assert_eq!(
            s(&r.call(&["GET", src])),
            "$15\\r\\nvalue-of-source\\r\\n",
            "COPY removed its source {src}"
        );
    }
    assert!(lost.is_empty(), "{} of {} copies never reached the destination:\n  {}",
        lost.len(), PAIRS.len(), lost.join("\n  "));
}

#[test]
fn the_ttl_travels_with_the_value() {
    let server = shared();
    let mut w = server.wire();
    for (src, dst) in PAIRS.iter().take(6) {
        let (src, dst) = (format!("ttl-{src}"), format!("ttl-{dst}"));
        assert_eq!(s(&w.call(&["SET", &src, "v"])), "+OK\\r\\n");
        assert_eq!(s(&w.call(&["EXPIRE", &src, "10000"])), ":1\\r\\n");
        assert_eq!(s(&w.call(&["COPY", &src, &dst])), ":1\\r\\n");
        // Not the exact remaining time — that is a clock read, and two
        // of them are two different questions. What must hold is that
        // the destination has a deadline at all: `-1` would mean the
        // copy arrived without one.
        let ttl = s(&w.call(&["TTL", &dst]));
        assert!(
            ttl != "-1\\r\\n" && ttl != ":-1\\r\\n",
            "{dst} arrived without the source's deadline: TTL says {ttl}"
        );
    }
}

#[test]
fn an_existing_destination_is_refused_until_replace_says_otherwise() {
    let server = shared();
    let mut w = server.wire();
    for (src, dst) in PAIRS.iter().take(6) {
        let (src, dst) = (format!("rep-{src}"), format!("rep-{dst}"));
        assert_eq!(s(&w.call(&["SET", &src, "from-source"])), "+OK\\r\\n");
        assert_eq!(s(&w.call(&["SET", &dst, "already-here"])), "+OK\\r\\n");

        assert_eq!(s(&w.call(&["COPY", &src, &dst])), ":0\\r\\n", "COPY overwrote {dst}");
        // The refusal must leave the destination untouched — and the
        // source too, which is the whole reason this family needs no
        // rollback step.
        assert_eq!(s(&w.call(&["GET", &dst])), "$12\\r\\nalready-here\\r\\n");
        assert_eq!(s(&w.call(&["GET", &src])), "$11\\r\\nfrom-source\\r\\n");

        assert_eq!(s(&w.call(&["COPY", &src, &dst, "REPLACE"])), ":1\\r\\n");
        assert_eq!(s(&w.call(&["GET", &dst])), "$11\\r\\nfrom-source\\r\\n");
        assert_eq!(s(&w.call(&["GET", &src])), "$11\\r\\nfrom-source\\r\\n");
    }
}

#[test]
fn a_missing_source_copies_nothing_and_says_so() {
    let server = shared();
    let mut w = server.wire();
    for (src, dst) in PAIRS.iter().take(6) {
        let (src, dst) = (format!("gone-{src}"), format!("gone-{dst}"));
        assert_eq!(s(&w.call(&["COPY", &src, &dst])), ":0\\r\\n");
        assert_eq!(s(&w.call(&["EXISTS", &dst])), ":0\\r\\n", "{dst} was created from nothing");
    }
}

#[test]
fn the_refusals_are_redis_words() {
    let server = shared();
    let mut w = server.wire();
    assert_eq!(
        s(&w.call(&["COPY", "same", "same"])),
        "-ERR source and destination objects are the same\\r\\n"
    );
    assert_eq!(s(&w.call(&["COPY", "only-one"])), "-ERR wrong number of arguments for 'copy' command\\r\\n");
    assert_eq!(s(&w.call(&["COPY", "a", "b", "REPLACED"])), "-ERR syntax error\\r\\n");
    // And the connection is still usable after each — a reply that
    // never filled its slot would stall every command behind it.
    assert_eq!(s(&w.call(&["PING"])), "+PONG\\r\\n");
}

#[test]
fn a_copied_collection_arrives_whole() {
    let server = shared();
    let mut w = server.wire();
    for (src, dst) in PAIRS.iter().take(6) {
        let (src, dst) = (format!("list-{src}"), format!("list-{dst}"));
        assert_eq!(s(&w.call(&["RPUSH", &src, "a", "b", "c"])), ":3\\r\\n");
        assert_eq!(s(&w.call(&["COPY", &src, &dst])), ":1\\r\\n");
        assert_eq!(
            s(&w.call(&["LRANGE", &dst, "0", "-1"])),
            "*3\\r\\n$1\\r\\na\\r\\n$1\\r\\nb\\r\\n$1\\r\\nc\\r\\n",
            "{dst} did not receive the whole list"
        );
        // Independent copies: pushing to one must not move the other.
        assert_eq!(s(&w.call(&["RPUSH", &dst, "d"])), ":4\\r\\n");
        assert_eq!(s(&w.call(&["LLEN", &src])), ":3\\r\\n", "{src} grew with its copy");
    }
}

#[test]
fn a_cross_shard_copy_survives_a_restart() {
    // Its own server: this one stops the process it is testing.
    let server = Server::start();
    let mut w = server.wire();
    let pairs: Vec<(String, String)> = PAIRS
        .iter()
        .filter(|(a, b)| {
            kevy_rt::shard_of_key(a.as_bytes(), SHARDS, false)
                != kevy_rt::shard_of_key(b.as_bytes(), SHARDS, false)
        })
        .map(|(a, b)| (format!("dur-{a}"), format!("dur-{b}")))
        .collect();
    assert!(pairs.len() >= 6, "only {} cross-shard pairs to test with", pairs.len());

    for (src, dst) in &pairs {
        assert_eq!(s(&w.call(&["SET", src, "written-down"])), "+OK\\r\\n");
        assert_eq!(s(&w.call(&["COPY", src, dst])), ":1\\r\\n");
    }
    drop(w);

    let server = server.restart();
    let mut w = server.wire();
    let mut missing = Vec::new();
    for (src, dst) in &pairs {
        if w.call(&["GET", dst]) != b"$12\r\nwritten-down\r\n" {
            missing.push(dst.clone());
        }
        assert_eq!(s(&w.call(&["GET", src])), "$12\\r\\nwritten-down\\r\\n", "{src} did not survive");
    }
    assert!(
        missing.is_empty(),
        "{} of {} cross-shard copies were remembered but never written down: {missing:?}",
        missing.len(),
        pairs.len()
    );
}
