//! What the stream writes whose record depends on how they ran cost a
//! server that records nothing (AOF off, no replicas), counted rather than
//! timed. Their records are built only where there is somewhere to write
//! them, so here they cost no more than a write recorded as typed:
//!
//! * an `XADD` with a generated ID costs what one with an explicit ID does;
//! * an `XREADGROUP` that delivers costs no more than it did while it was
//!   recorded as its own argv.
//!
//! The count is process-wide while a window is open, so this file holds
//! one test: nothing else runs beside it. The client side allocates
//! nothing inside a window.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use kevy_testnet::free_port;

static ON: AtomicBool = AtomicBool::new(false);
static ALLOCS: AtomicU64 = AtomicU64::new(0);

struct Counting;

// SAFETY: every method forwards to `System`, which is a correct allocator;
// the counters are atomics and touch no allocation.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if ON.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: `l` came from the caller and is forwarded unchanged.
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: `p`/`l` came from the caller and are forwarded unchanged.
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        if ON.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: forwarded unchanged.
        unsafe { System.realloc(p, l, new) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn req(parts: &[&str]) -> Vec<u8> {
    let mut v = format!("*{}\r\n", parts.len()).into_bytes();
    for p in parts {
        v.extend_from_slice(format!("${}\r\n{p}\r\n", p.len()).as_bytes());
    }
    v
}

/// Where one whole RESP2 reply starting at `at` ends, if `b` holds it.
fn reply_end(b: &[u8], at: usize) -> Option<usize> {
    let eol = at + b.get(at..)?.windows(2).position(|w| w == b"\r\n")?;
    let n: i64 = std::str::from_utf8(&b[at + 1..eol]).ok()?.parse().unwrap_or(0);
    match b[at] {
        b'$' if n >= 0 => {
            let end = eol + 2 + n as usize + 2;
            (end <= b.len()).then_some(end)
        }
        b'*' => (0..n.max(0)).try_fold(eol + 2, |pos, _| reply_end(b, pos)),
        _ => Some(eol + 2),
    }
}

/// Send each request and read its reply into a stack buffer; `want` is
/// the reply's first byte.
fn send_all(c: &mut std::net::TcpStream, reqs: &[Vec<u8>], want: u8) {
    let mut buf = [0u8; 512];
    for r in reqs {
        c.write_all(r).unwrap();
        let mut n = 0;
        while reply_end(&buf[..n], 0).is_none() {
            n += c.read(&mut buf[n..]).unwrap();
        }
        assert_eq!(buf[0], want, "{:?}", String::from_utf8_lossy(&buf[..n]));
    }
}

/// Allocations, process-wide, while `reqs` are answered.
fn count(c: &mut std::net::TcpStream, reqs: &[Vec<u8>], want: u8) -> u64 {
    ALLOCS.store(0, Ordering::SeqCst);
    ON.store(true, Ordering::SeqCst);
    send_all(c, reqs, want);
    ON.store(false, Ordering::SeqCst);
    ALLOCS.load(Ordering::SeqCst)
}

const N: usize = 500;

/// XADDs of `e` with explicit IDs as long as generated ones, whose reply
/// is formatted the same way.
fn explicit(from: usize) -> Vec<Vec<u8>> {
    let id = |i: usize| format!("{}-0", 1_000_000_000_000 + i);
    (from..from + N).map(|i| req(&["XADD", "e", &id(i), "f", "v"])).collect()
}

/// Rounds, each explicit then generated with both streams the same length,
/// so the stream's own node splits fall alike. The count is process-wide:
/// now and then a runtime tick adds to one window, never to every round,
/// so the rounds must agree at least once, and a record built for nothing
/// adds N or more to every round.
fn xadd_rounds(c: &mut std::net::TcpStream) -> Vec<(u64, u64)> {
    let generated: Vec<Vec<u8>> = (0..N).map(|_| req(&["XADD", "g", "*", "f", "v"])).collect();
    send_all(c, &explicit(0), b'$');
    send_all(c, &generated, b'$');
    (1..=5)
        .map(|round| {
            let e = explicit(round * N);
            (count(c, &e, b'$'), count(c, &generated, b'$'))
        })
        .collect()
}

/// Allocations for N `XREADGROUP … COUNT 1 STREAMS e >`, each delivering
/// one entry, by round; the least is the commands' own.
fn xreadgroup_rounds(c: &mut std::net::TcpStream) -> Vec<u64> {
    send_all(c, &[req(&["XGROUP", "CREATE", "e", "grp", "0"])], b'+');
    let read: Vec<Vec<u8>> = (0..N)
        .map(|_| req(&["XREADGROUP", "GROUP", "grp", "c", "COUNT", "1", "STREAMS", "e", ">"]))
        .collect();
    send_all(c, &read, b'*');
    (1..=5).map(|_| count(c, &read, b'*')).collect()
}

/// Allocations for N `XREADGROUP`s that deliver one entry each, measured
/// here while the command was recorded as its own argv, before its record
/// became the outcome it produced. The outcome-shaped record is built only
/// where a write is recorded, so here it must not cost more.
/// By round: the rounds differ because the pending list and the stream shrink
/// and grow as the reads go on.
const XREADGROUP_BEFORE: [u64; 5] = [16537, 16082, 16061, 15570, 14574];

#[test]
fn records_cost_nothing_where_nothing_records() {
    let port = free_port();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_t = stop.clone();
    let handle = std::thread::spawn(move || {
        kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(1))
            .bind([127, 0, 0, 1], port)
            .shards(1)
            .with_aof(false)
            .run(stop_t)
            .unwrap();
    });
    let up = (0..400).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
    });
    assert!(up, "runtime did not start");
    let mut c = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    let xadd = xadd_rounds(&mut c);
    let xread = xreadgroup_rounds(&mut c);
    println!("allocations per {N} XADDs, AOF off, (explicit, generated) by round: {xadd:?}");
    println!("allocations per {N} delivering XREADGROUPs, AOF off, by round: {xread:?}");
    stop.store(true, Ordering::Relaxed);
    let _ = handle.join();
    assert!(xadd.iter().all(|r| r.0 >= N as u64), "the count saw nothing: {xadd:?}");
    let closest = xadd.iter().map(|(id, star)| id.abs_diff(*star)).min();
    assert_eq!(closest, Some(0), "a generated ID allocates more than an explicit one: {xadd:?}");
    assert!(xread.iter().all(|&n| n >= N as u64), "the count saw nothing: {xread:?}");
    // a tick lands in a window now and then, never in every one
    let within = xread.iter().zip(XREADGROUP_BEFORE).any(|(&now, before)| now <= before);
    assert!(within, "XREADGROUP allocates more than {XREADGROUP_BEFORE:?}: {xread:?}");
}
