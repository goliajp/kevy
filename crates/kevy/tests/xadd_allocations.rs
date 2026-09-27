//! What an `XADD` with a generated ID costs a server that records nothing
//! (AOF off, no replicas), counted rather than timed: the same as one with
//! an explicit ID. Its record — the argv with the ID it gave — is built
//! only where there is somewhere to write it.
//!
//! The count is process-wide while the window is open, so this file holds
//! one test: nothing else runs beside it. The client side is written so it
//! allocates nothing inside a window.

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

/// Send each request and read its bulk reply into a stack buffer.
fn send_all(c: &mut std::net::TcpStream, reqs: &[Vec<u8>]) {
    let mut buf = [0u8; 256];
    for r in reqs {
        c.write_all(r).unwrap();
        let mut n = 0;
        loop {
            n += c.read(&mut buf[n..]).unwrap();
            let lines = buf[..n].windows(2).filter(|w| w == b"\r\n").count();
            if buf[0] != b'$' || lines >= 2 {
                break;
            }
        }
        assert_eq!(buf[0], b'$', "{:?}", String::from_utf8_lossy(&buf[..n]));
    }
}

/// Allocations, process-wide, while `reqs` are answered.
fn count(c: &mut std::net::TcpStream, reqs: &[Vec<u8>]) -> u64 {
    ALLOCS.store(0, Ordering::SeqCst);
    ON.store(true, Ordering::SeqCst);
    send_all(c, reqs);
    ON.store(false, Ordering::SeqCst);
    ALLOCS.load(Ordering::SeqCst)
}

#[test]
fn a_generated_id_costs_what_an_explicit_one_does_when_nothing_records() {
    const N: usize = 500;
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
    let explicit = |from: usize| -> Vec<Vec<u8>> {
        // as long as a generated ID, whose reply is formatted the same way
        let id = |i: usize| format!("{}-0", 1_000_000_000_000 + i);
        (from..from + N).map(|i| req(&["XADD", "e", &id(i), "f", "v"])).collect()
    };
    let generated: Vec<Vec<u8>> = (0..N).map(|_| req(&["XADD", "g", "*", "f", "v"])).collect();
    // warm both streams and the connection's buffers past their growth
    send_all(&mut c, &explicit(0));
    send_all(&mut c, &generated);
    // Rounds, each explicit then generated with both streams the same
    // length, so the stream's own node splits fall alike. The count is
    // process-wide: now and then a runtime tick adds to one window, never
    // to every round, so the rounds must agree at least once, and a record
    // built for nothing would add N or more to every round.
    let rounds: Vec<(u64, u64)> = (1..=5)
        .map(|round| {
            let e = explicit(round * N);
            (count(&mut c, &e), count(&mut c, &generated))
        })
        .collect();
    println!("allocations per {N} XADDs, AOF off, (explicit, generated) by round: {rounds:?}");
    stop.store(true, Ordering::Relaxed);
    let _ = handle.join();
    assert!(rounds.iter().all(|r| r.0 >= N as u64), "the count saw nothing: {rounds:?}");
    let closest = rounds.iter().map(|(id, star)| id.abs_diff(*star)).min();
    assert_eq!(closest, Some(0), "a generated ID allocates more than an explicit one: {rounds:?}");
}
