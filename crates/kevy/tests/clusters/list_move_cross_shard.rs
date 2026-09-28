//! `RPOPLPUSH` / `LMOVE` / `BRPOPLPUSH` on a MULTI-SHARD server.
//!
//! These verbs move an element between two keys, and on a thread-per-core
//! server the two keys routinely live on different cores. Until v4 they fell
//! through the router's catch-all (`Route::Single(1)` — hash args[1], the
//! source) and executed entirely on the source's shard, push included. The
//! element landed in a keyspace nobody would ever read it from, while the
//! command returned the moved value, so the caller believed it had worked.
//! Measured on 8 shards: RPOPLPUSH lost 11 of 12 elements, BRPOPLPUSH 9 of 12.
//!
//! It survived to v4 because every list test in this repo tested a single
//! `Store` (`bullmq_list.rs` dispatches straight at the store) or a
//! single-shard server. A one-shard server cannot see this class of bug at
//! all: both keys are always co-located. **Every test in this file therefore
//! runs on eight shards, and every one of them asserts the DESTINATION's
//! contents — never just the reply.** A test that only checked the reply would
//! have passed against the broken code.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

static START_GATE: Mutex<()> = Mutex::new(());

/// Eight shards: enough that a dozen arbitrary key pairs are almost never
/// co-located, which is exactly the condition the bug needed.
const SHARDS: usize = 8;

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

/// Read one whole RESP reply. Enough of a parser for the shapes here:
/// simple string, error, integer, bulk (incl. nil), array.
fn read_reply(c: &mut std::net::TcpStream) -> Vec<u8> {
    let mut out = Vec::new();
    let mut byte = [0u8; 1];
    // header line
    loop {
        c.read_exact(&mut byte).unwrap();
        out.push(byte[0]);
        if out.len() >= 2 && out[out.len() - 2] == b'\r' && out[out.len() - 1] == b'\n' {
            break;
        }
    }
    match out[0] {
        b'+' | b'-' | b':' => out,
        b'$' => {
            let n: i64 = std::str::from_utf8(&out[1..out.len() - 2]).unwrap().parse().unwrap();
            if n < 0 {
                return out;
            }
            let mut body = vec![0u8; n as usize + 2];
            c.read_exact(&mut body).unwrap();
            out.extend_from_slice(&body);
            out
        }
        b'*' => {
            let n: i64 = std::str::from_utf8(&out[1..out.len() - 2]).unwrap().parse().unwrap();
            for _ in 0..n.max(0) {
                out.extend_from_slice(&read_reply(c));
            }
            out
        }
        _ => out,
    }
}

fn call(c: &mut std::net::TcpStream, parts: &[&[u8]]) -> Vec<u8> {
    c.write_all(&req(parts)).unwrap();
    read_reply(c)
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
            "kevy-listmove-{}",
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
        s.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
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

// ── the regression the whole file exists for ────────────────────────────────

/// Twelve arbitrary key pairs across eight shards. Before the fix this lost
/// eleven of them — and reported success on every one.
#[test]
fn rpoplpush_lands_the_element_on_the_destinations_shard() {
    let srv = Server::start(SHARDS);
    let mut c = srv.connect();
    for i in 0..12 {
        let src = format!("src{i}");
        let dst = format!("dst{i}");
        let job = format!("job-{i}");
        call(&mut c, &[b"DEL", src.as_bytes(), dst.as_bytes()]);
        call(&mut c, &[b"RPUSH", src.as_bytes(), job.as_bytes()]);

        let moved = call(&mut c, &[b"RPOPLPUSH", src.as_bytes(), dst.as_bytes()]);
        assert_eq!(
            moved,
            format!("${}\r\n{job}\r\n", job.len()).into_bytes(),
            "pair {i}: the move should return the element"
        );

        // The assertion that matters: it is actually THERE.
        assert_eq!(
            call(&mut c, &[b"LLEN", dst.as_bytes()]),
            b":1\r\n",
            "pair {i}: element vanished — the push went to the wrong shard"
        );
        assert_eq!(
            call(&mut c, &[b"LINDEX", dst.as_bytes(), b"0"]),
            format!("${}\r\n{job}\r\n", job.len()).into_bytes(),
            "pair {i}: destination holds the wrong element"
        );
        assert_eq!(
            call(&mut c, &[b"LLEN", src.as_bytes()]),
            b":0\r\n",
            "pair {i}: source not drained"
        );
    }
}

#[test]
fn brpoplpush_lands_the_element_on_the_destinations_shard() {
    let srv = Server::start(SHARDS);
    let mut c = srv.connect();
    for i in 0..12 {
        let src = format!("bs{i}");
        let dst = format!("bd{i}");
        let job = format!("job-{i}");
        call(&mut c, &[b"DEL", src.as_bytes(), dst.as_bytes()]);
        call(&mut c, &[b"RPUSH", src.as_bytes(), job.as_bytes()]);

        // The source already holds an element, so this must serve immediately.
        let moved = call(&mut c, &[b"BRPOPLPUSH", src.as_bytes(), dst.as_bytes(), b"2"]);
        assert_eq!(moved, format!("${}\r\n{job}\r\n", job.len()).into_bytes(), "pair {i}");
        assert_eq!(
            call(&mut c, &[b"LLEN", dst.as_bytes()]),
            b":1\r\n",
            "pair {i}: element vanished on the blocking path"
        );
    }
}

/// All four LMOVE directions, cross-shard.
#[test]
fn lmove_honours_both_ends_across_shards() {
    let srv = Server::start(SHARDS);
    let mut c = srv.connect();
    for (from, to, expect) in [
        (&b"LEFT"[..], &b"LEFT"[..], "a"),
        (&b"LEFT"[..], &b"RIGHT"[..], "a"),
        (&b"RIGHT"[..], &b"LEFT"[..], "c"),
        (&b"RIGHT"[..], &b"RIGHT"[..], "c"),
    ] {
        call(&mut c, &[b"DEL", b"mv:src", b"mv:dst"]);
        call(&mut c, &[b"RPUSH", b"mv:src", b"a", b"b", b"c"]);
        let moved = call(&mut c, &[b"LMOVE", b"mv:src", b"mv:dst", from, to]);
        assert_eq!(moved, format!("${}\r\n{expect}\r\n", expect.len()).into_bytes());
        assert_eq!(
            call(&mut c, &[b"LRANGE", b"mv:dst", b"0", b"-1"]),
            format!("*1\r\n${}\r\n{expect}\r\n", expect.len()).into_bytes(),
            "LMOVE {from:?} {to:?}: destination wrong"
        );
    }
}

/// A wrong-type destination must not cost the caller their element. The
/// cross-shard path pops before it can know the destination's type, so it has
/// to put the element back.
#[test]
fn a_wrongtype_destination_gives_the_element_back() {
    let srv = Server::start(SHARDS);
    let mut c = srv.connect();
    call(&mut c, &[b"DEL", b"wt:src", b"wt:dst"]);
    call(&mut c, &[b"RPUSH", b"wt:src", b"precious"]);
    call(&mut c, &[b"SET", b"wt:dst", b"not-a-list"]);

    let r = call(&mut c, &[b"RPOPLPUSH", b"wt:src", b"wt:dst"]);
    assert!(r.starts_with(b"-WRONGTYPE"), "expected WRONGTYPE, got {r:?}");
    assert_eq!(
        call(&mut c, &[b"LRANGE", b"wt:src", b"0", b"-1"]),
        b"*1\r\n$8\r\nprecious\r\n",
        "the element must be back on the source"
    );
}

#[test]
fn a_wrongtype_destination_gives_the_element_back_on_the_blocking_path() {
    let srv = Server::start(SHARDS);
    let mut c = srv.connect();
    call(&mut c, &[b"DEL", b"bwt:src", b"bwt:dst"]);
    call(&mut c, &[b"RPUSH", b"bwt:src", b"precious"]);
    call(&mut c, &[b"SET", b"bwt:dst", b"not-a-list"]);

    let r = call(&mut c, &[b"BRPOPLPUSH", b"bwt:src", b"bwt:dst", b"2"]);
    assert!(r.starts_with(b"-WRONGTYPE"), "expected WRONGTYPE, got {r:?}");
    assert_eq!(call(&mut c, &[b"LRANGE", b"bwt:src", b"0", b"-1"]), b"*1\r\n$8\r\nprecious\r\n");
}

/// An empty source must not conjure the destination into existence.
#[test]
fn an_empty_source_replies_nil_and_leaves_the_destination_absent() {
    let srv = Server::start(SHARDS);
    let mut c = srv.connect();
    call(&mut c, &[b"DEL", b"e:src", b"e:dst"]);
    assert_eq!(call(&mut c, &[b"RPOPLPUSH", b"e:src", b"e:dst"]), b"$-1\r\n");
    assert_eq!(call(&mut c, &[b"EXISTS", b"e:dst"]), b":0\r\n");
}

/// `{hashtag}`-co-located keys take the same-shard path, which is atomic —
/// this is the documented way to get Redis's RPOPLPUSH guarantee back.
#[test]
fn co_located_keys_still_take_the_atomic_same_shard_path() {
    let srv = Server::start(SHARDS);
    let mut c = srv.connect();
    call(&mut c, &[b"DEL", b"{q}:src", b"{q}:dst"]);
    call(&mut c, &[b"RPUSH", b"{q}:src", b"x"]);
    assert_eq!(call(&mut c, &[b"RPOPLPUSH", b"{q}:src", b"{q}:dst"]), b"$1\r\nx\r\n");
    assert_eq!(call(&mut c, &[b"LLEN", b"{q}:dst"]), b":1\r\n");
}

/// `src == dst` is a rotation, and it must stay one on a sharded server.
#[test]
fn source_equals_destination_rotates() {
    let srv = Server::start(SHARDS);
    let mut c = srv.connect();
    call(&mut c, &[b"DEL", b"rot"]);
    call(&mut c, &[b"RPUSH", b"rot", b"a", b"b", b"c"]);
    assert_eq!(call(&mut c, &[b"RPOPLPUSH", b"rot", b"rot"]), b"$1\r\nc\r\n");
    assert_eq!(
        call(&mut c, &[b"LRANGE", b"rot", b"0", b"-1"]),
        b"*3\r\n$1\r\nc\r\n$1\r\na\r\n$1\r\nb\r\n"
    );
}

/// The blocking form must still block, then serve the element that arrives —
/// and land it on the right shard.
#[test]
fn brpoplpush_parks_then_serves_a_later_push_to_the_right_shard() {
    let srv = Server::start(SHARDS);
    let mut waiter = srv.connect();
    let mut pusher = srv.connect();
    call(&mut pusher, &[b"DEL", b"blk:src", b"blk:dst"]);

    waiter.write_all(&req(&[b"BRPOPLPUSH", b"blk:src", b"blk:dst", b"5"])).unwrap();
    // Give it time to actually park before the element shows up.
    std::thread::sleep(std::time::Duration::from_millis(300));
    call(&mut pusher, &[b"RPUSH", b"blk:src", b"late-job"]);

    assert_eq!(read_reply(&mut waiter), b"$8\r\nlate-job\r\n");
    assert_eq!(call(&mut pusher, &[b"LLEN", b"blk:dst"]), b":1\r\n");
    assert_eq!(call(&mut pusher, &[b"LINDEX", b"blk:dst", b"0"]), b"$8\r\nlate-job\r\n");
}

/// A blocking move that never gets an element times out with nil — and does
/// not leave the destination behind.
#[test]
fn brpoplpush_times_out_with_nil() {
    let srv = Server::start(SHARDS);
    let mut c = srv.connect();
    call(&mut c, &[b"DEL", b"to:src", b"to:dst"]);
    let t0 = std::time::Instant::now();
    assert_eq!(call(&mut c, &[b"BRPOPLPUSH", b"to:src", b"to:dst", b"1"]), b"$-1\r\n");
    let dt = t0.elapsed();
    assert!(dt >= std::time::Duration::from_millis(800), "returned too early: {dt:?}");
    assert_eq!(call(&mut c, &[b"EXISTS", b"to:dst"]), b":0\r\n");
}

/// A full BullMQ-shaped drain: N jobs through a cross-shard move, none lost.
/// This is the workload the code comments name, and the one that was broken.
#[test]
fn a_queue_drain_moves_every_job_exactly_once() {
    let srv = Server::start(SHARDS);
    let mut c = srv.connect();
    call(&mut c, &[b"DEL", b"queue:wait", b"queue:active"]);
    for i in 0..50 {
        let job = format!("job-{i}");
        call(&mut c, &[b"LPUSH", b"queue:wait", job.as_bytes()]);
    }
    let mut drained = Vec::new();
    for _ in 0..50 {
        let r = call(&mut c, &[b"RPOPLPUSH", b"queue:wait", b"queue:active"]);
        assert!(!r.starts_with(b"$-1"), "queue drained early — a job was lost");
        drained.push(r);
    }
    assert_eq!(call(&mut c, &[b"LLEN", b"queue:wait"]), b":0\r\n");
    assert_eq!(
        call(&mut c, &[b"LLEN", b"queue:active"]),
        b":50\r\n",
        "every job must be in the active list"
    );
    assert_eq!(call(&mut c, &[b"RPOPLPUSH", b"queue:wait", b"queue:active"]), b"$-1\r\n");
}
