//! `BITOP op dst src [src …]` on a MULTI-SHARD server.
//!
//! Three shard-crossings in one command: the sources are read where
//! they live, the bytes are combined on the shard that took the
//! command, and the result is written where the destination lives.
//! `args[1]` is the OPERATOR, so the router's catch-all would have
//! hashed the word "AND" and run everything on whatever shard that
//! landed on — a destination written into a keyspace no later read of
//! it would look at, with the reply reporting a length.
//!
//! So, the rule `list_move_cross_shard.rs` wrote after RPOPLPUSH lost
//! 11 of 12 elements: **assert what the destination holds.** Every test
//! here reads the destination back, and the byte values are worked out
//! by hand rather than taken from the engine.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

mod common;

static START_GATE: Mutex<()> = Mutex::new(());
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
            "kevy-bitopxshard-{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
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
        kevy_testnet::assert_listening(port, "the cross-shard BITOP server");
        Self { port, dir, stop, handle: Some(handle) }
    }

    fn wire(&self) -> common::Wire {
        let sock = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        sock.set_read_timeout(Some(std::time::Duration::from_secs(8))).unwrap();
        common::Wire::new(sock)
    }
}

impl Server {
    /// Stop and bring another up on the same data directory.
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
        kevy_testnet::assert_listening(port, "the restarted BITOP server");
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

/// One server for the whole file: these tests use disjoint keys, and
/// the workspace suite already saturates `free_port`'s window.
fn shared() -> &'static Server {
    static S: std::sync::OnceLock<Server> = std::sync::OnceLock::new();
    S.get_or_init(Server::start)
}

fn s(b: &[u8]) -> String {
    String::from_utf8_lossy(b).replace("\r\n", "\\r\\n")
}

/// Three source keys on three different shards, in a namespace of the
/// caller's own.
///
/// Two things this had to learn. The keys are CHOSEN by asking the
/// function the server routes with, not picked and hoped for — the
/// first draft hard-coded three names and the floor caught two of them
/// sharing shard 7 on the first run. And the namespace is per-test,
/// because these tests share one server and cargo runs them in
/// parallel: a common set of seeded keys passed locally and raced in
/// CI, where three of them failed on a DEL that answered :1 because
/// another test had just written the key.
fn sources(tag: &str) -> [String; 3] {
    let of = |k: &str| kevy_rt::shard_of_key(k.as_bytes(), SHARDS, false);
    let mut picked: Vec<String> = Vec::new();
    for i in 0..4000 {
        let k = format!("bop-{tag}-{i}");
        if picked.iter().all(|p| of(p) != of(&k)) {
            picked.push(k);
        }
        if picked.len() == 3 {
            break;
        }
    }
    assert_eq!(picked.len(), 3, "no three `bop-{tag}-*` names landed on three shards");
    [picked[0].clone(), picked[1].clone(), picked[2].clone()]
}

/// A bulk-string reply carrying exactly these bytes. Built as BYTES:
/// the first draft wrote `format!("$1\r\n{}\r\n", byte as char)`, and
/// `0x80 as char` is U+0080, which UTF-8 encodes as two bytes — so the
/// test reported the engine wrong for a mistake in its own expectation.
fn bulk(bytes: &[u8]) -> Vec<u8> {
    let mut v = format!("${}\r\n", bytes.len()).into_bytes();
    v.extend_from_slice(bytes);
    v.extend_from_slice(b"\r\n");
    v
}

/// `SETBIT k 0 1` sets the most significant bit of byte 0 — 0x80.
/// `SETBIT k 7 1` sets the least — 0x01. Seeds A=0x81, B=0x90, C=0x01.
fn seed(w: &mut common::Wire, keys: &[String; 3]) {
    for (k, bits) in keys.iter().zip([[0u8, 7], [0, 3], [7, 7]]) {
        for bit in bits {
            let _ = w.call(&["SETBIT", k, &bit.to_string(), "1"]);
        }
    }
}

#[test]
fn the_keys_this_file_uses_are_really_on_different_shards() {
    let of = |k: &str| kevy_rt::shard_of_key(k.as_bytes(), SHARDS, false);
    for tag in ["and", "not", "wrong", "dur"] {
        let k = sources(tag);
        let (x, y, z) = (of(&k[0]), of(&k[1]), of(&k[2]));
        println!("{tag}: {} {} {} → shards {x} {y} {z}", k[0], k[1], k[2]);
        assert!(x != y && y != z && x != z, "the `{tag}` sources share a shard");
    }
}

#[test]
fn and_or_xor_land_at_the_destination_with_the_bytes_worked_out_by_hand() {
    let mut w = shared().wire();
    let k = sources("and");
    seed(&mut w, &k);
    let (a, b, c) = (k[0].as_str(), k[1].as_str(), k[2].as_str());
    // A = 0x81 (bits 0 and 7), B = 0x90 (bits 0 and 3), C = 0x01 (bit 7).
    for (op, dst, want) in [
        ("AND", "bop-and", 0x81u8 & 0x90),
        ("OR", "bop-or", 0x81 | 0x90),
        ("XOR", "bop-xor", 0x81 ^ 0x90),
    ] {
        assert_eq!(s(&w.call(&["BITOP", op, dst, a, b])), ":1\\r\\n", "{op} length");
        let got = w.call(&["GET", dst]);
        assert_eq!(got, bulk(&[want]), "{op} wrote {:?} at {dst}, wanted {want:#04x}", s(&got));
    }
    // Three sources, all on different shards.
    assert_eq!(s(&w.call(&["BITOP", "OR", "bop-or3", a, b, c])), ":1\\r\\n");
    assert_eq!(w.call(&["GET", "bop-or3"]), bulk(&[0x81 | 0x90 | 0x01]));
}

#[test]
fn not_inverts_a_source_that_lives_elsewhere() {
    let mut w = shared().wire();
    let k = sources("not");
    seed(&mut w, &k);
    assert_eq!(s(&w.call(&["BITOP", "NOT", "bop-not", &k[0]])), ":1\\r\\n");
    assert_eq!(w.call(&["GET", "bop-not"]), bulk(&[!0x81u8]));
}

#[test]
fn an_empty_result_deletes_the_destination_rather_than_storing_nothing() {
    let mut w = shared().wire();
    assert_eq!(s(&w.call(&["SET", "bop-victim", "was-here"])), "+OK\\r\\n");
    // Both sources absent, and each on its own shard.
    assert_eq!(s(&w.call(&["BITOP", "AND", "bop-victim", "bop-gone-1", "bop-gone-2"])), ":0\\r\\n");
    assert_eq!(
        s(&w.call(&["EXISTS", "bop-victim"])),
        ":0\\r\\n",
        "an empty BITOP left a zero-length string where Redis deletes the key"
    );
}

#[test]
fn a_wrong_typed_source_refuses_the_whole_command_and_writes_nothing() {
    let mut w = shared().wire();
    let k = sources("wrong");
    seed(&mut w, &k);
    assert_eq!(s(&w.call(&["RPUSH", "bop-wrong-list", "x"])), ":1\\r\\n");
    assert_eq!(
        s(&w.call(&["BITOP", "AND", "bop-refused", &k[0], "bop-wrong-list"])),
        "-WRONGTYPE Operation against a key holding the wrong kind of value\\r\\n"
    );
    assert_eq!(
        s(&w.call(&["EXISTS", "bop-refused"])),
        ":0\\r\\n",
        "a refused BITOP still wrote its destination"
    );
    // And the connection is usable: a reply that never filled its slot
    // would stall everything queued behind it.
    assert_eq!(s(&w.call(&["PING"])), "+PONG\\r\\n");
}

#[test]
fn the_refusals_are_redis_words() {
    let mut w = shared().wire();
    assert_eq!(s(&w.call(&["BITOP"])), "-ERR wrong number of arguments for 'bitop' command\\r\\n");
    assert_eq!(s(&w.call(&["BITOP", "AND", "d"])), "-ERR wrong number of arguments for 'bitop' command\\r\\n");
    assert_eq!(s(&w.call(&["BITOP", "SIDEWAYS", "d", "a"])), "-ERR syntax error\\r\\n");
    assert_eq!(
        s(&w.call(&["BITOP", "NOT", "d", "a", "b"])),
        "-ERR BITOP NOT must be called with a single source key.\\r\\n"
    );
    assert_eq!(s(&w.call(&["PING"])), "+PONG\\r\\n");
}

#[test]
fn the_longest_source_sets_the_length_and_the_short_ones_pad_with_zero() {
    let mut w = shared().wire();
    assert_eq!(s(&w.call(&["SET", "bop-long", "abcd"])), "+OK\\r\\n");
    assert_eq!(s(&w.call(&["SET", "bop-short", "ab"])), "+OK\\r\\n");
    assert_eq!(s(&w.call(&["BITOP", "OR", "bop-padded", "bop-long", "bop-short"])), ":4\\r\\n");
    let want: Vec<u8> = b"abcd".iter().zip(b"ab\0\0").map(|(x, y)| x | y).collect();
    assert_eq!(w.call(&["GET", "bop-padded"]), bulk(&want), "the short source did not zero-pad");
}

/// Both halves of what a cross-shard BITOP writes down: the stored
/// result, and the DELETE an empty result performs.
///
/// The delete is the half worth asserting. `Op::BitOpResult` logs `SET`
/// or `DEL` depending on what it did, and a version that logged only
/// the SET would pass every test above — the destination would simply
/// come back from the log after a restart, a key Redis had removed.
///
/// Compared as BYTES, not through `s()`: an expectation written with a
/// real CRLF and compared against a rendering that escapes it is a test
/// failing over its own punctuation.
#[test]
fn what_a_cross_shard_bitop_did_survives_a_restart_including_the_delete() {
    let server = Server::start();
    let mut w = server.wire();
    let k = sources("dur");
    for (key, bit) in [(&k[0], "0"), (&k[1], "3")] {
        assert_eq!(w.call(&["SETBIT", key, bit, "1"]), b":0\r\n");
    }
    assert_eq!(w.call(&["BITOP", "OR", "dur-or", &k[0], &k[1]]), b":1\r\n");
    // A destination that exists, then is emptied by a BITOP whose
    // sources are both absent.
    assert_eq!(w.call(&["SET", "dur-gone", "here-for-now"]), b"+OK\r\n");
    assert_eq!(w.call(&["BITOP", "AND", "dur-gone", "dur-nosuch-1", "dur-nosuch-2"]), b":0\r\n");
    assert_eq!(w.call(&["EXISTS", "dur-gone"]), b":0\r\n");
    drop(w);

    let server = server.restart();
    let mut w = server.wire();
    assert_eq!(
        w.call(&["GET", "dur-or"]),
        bulk(&[0x80 | 0x10]),
        "the cross-shard result was remembered but never written down"
    );
    assert_eq!(
        w.call(&["EXISTS", "dur-gone"]),
        b":0\r\n",
        "the destination an empty BITOP deleted came back from the log"
    );
}
