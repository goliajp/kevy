//! What a pipelined stream of forwarded single-key commands allocates on a
//! multi-shard server, counted rather than timed: in the steady state a
//! SET, GET or INCRBY that fits inline allocates nothing, whether it runs on
//! the connection's shard or is forwarded to the key's owner and back.
//!
//! The count is process-wide while a window is open, so this file holds
//! one test: nothing else runs beside it. The client side allocates
//! nothing inside a window.

#![allow(clippy::unwrap_used, clippy::panic)]

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

const SHARDS: usize = 4;
/// Commands per pipelined write.
const DEPTH: usize = 32;
const ROUNDS_PER_WINDOW: usize = 50;

fn req(parts: &[&str]) -> Vec<u8> {
    let mut v = format!("*{}\r\n", parts.len()).into_bytes();
    for p in parts {
        v.extend_from_slice(format!("${}\r\n{p}\r\n", p.len()).as_bytes());
    }
    v
}

/// One pipelined write of `DEPTH` commands over keys on every shard, and
/// the exact bytes its replies come back as.
fn pipeline(value: &str) -> (Vec<u8>, Vec<u8>) {
    let (mut out, mut want) = (Vec::new(), Vec::new());
    for i in 0..DEPTH {
        let key = format!("k{i}");
        match i % 3 {
            0 => {
                out.extend_from_slice(&req(&["SET", &key, value]));
                want.extend_from_slice(b"+OK\r\n");
            }
            1 => {
                out.extend_from_slice(&req(&["GET", &format!("k{}", i - 1)]));
                want.extend_from_slice(format!("${}\r\n{value}\r\n", value.len()).as_bytes());
            }
            _ => {
                out.extend_from_slice(&req(&["INCRBY", &format!("n{i}"), "0"]));
                want.extend_from_slice(b":0\r\n");
            }
        }
    }
    (out, want)
}

/// One pipelined write of `DEPTH` GETs over the keys [`pipeline`] sets,
/// each holding `value`, and their replies.
fn gets(value: &str) -> (Vec<u8>, Vec<u8>) {
    let (mut out, mut want) = (Vec::new(), Vec::new());
    for i in 0..DEPTH {
        out.extend_from_slice(&req(&["GET", &format!("k{}", i % DEPTH.div_ceil(3) * 3)]));
        want.extend_from_slice(format!("${}\r\n{value}\r\n", value.len()).as_bytes());
    }
    (out, want)
}

/// Send `rounds` pipelines and check every reply, into buffers allocated
/// before the call.
fn run(c: &mut std::net::TcpStream, reqs: &[u8], want: &[u8], got: &mut [u8], rounds: usize) {
    for _ in 0..rounds {
        c.write_all(reqs).unwrap();
        c.read_exact(got).unwrap();
        assert!(got == want, "{:?}", String::from_utf8_lossy(got));
    }
}

/// Allocations, process-wide, over one window of pipelines.
fn count(c: &mut std::net::TcpStream, reqs: &[u8], want: &[u8], got: &mut [u8]) -> u64 {
    ALLOCS.store(0, Ordering::SeqCst);
    ON.store(true, Ordering::SeqCst);
    run(c, reqs, want, got, ROUNDS_PER_WINDOW);
    ON.store(false, Ordering::SeqCst);
    ALLOCS.load(Ordering::SeqCst)
}

#[test]
fn forwarded_single_key_commands_allocate_nothing() {
    let port = free_port();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_t = stop.clone();
    let handle = std::thread::spawn(move || {
        kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(SHARDS))
            .bind([127, 0, 0, 1], port)
            .shards(SHARDS)
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
    // replies that come back short or out of order fail here, not hang
    c.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();

    let (reqs, want) = pipeline("v");
    let mut got = vec![0u8; want.len()];
    run(&mut c, &reqs, &want, &mut got, 20);
    let inline: Vec<u64> = (0..5).map(|_| count(&mut c, &reqs, &want, &mut got)).collect();

    // positive control: a value too long to store inline allocates once
    // per SET, so a counter that sees nothing fails here
    let long = "x".repeat(40);
    let (reqs, want) = pipeline(&long);
    let mut got = vec![0u8; want.len()];
    run(&mut c, &reqs, &want, &mut got, 20);
    let heap = count(&mut c, &reqs, &want, &mut got);
    // a reply too long for the inline arm, forwarded or not, allocates
    // nothing either: the owner writes it into the batch's buffer
    let (reqs, want) = gets(&long);
    let mut got = vec![0u8; want.len()];
    run(&mut c, &reqs, &want, &mut got, 20);
    let long_gets: Vec<u64> = (0..5).map(|_| count(&mut c, &reqs, &want, &mut got)).collect();

    println!("allocations per {ROUNDS_PER_WINDOW}×{DEPTH} commands, by window: {inline:?}");
    println!("with {}-byte values: {heap}; GETs only, by window: {long_gets:?}", long.len());
    stop.store(true, Ordering::Relaxed);
    let _ = handle.join();
    let sets = (ROUNDS_PER_WINDOW * DEPTH.div_ceil(3)) as u64;
    assert!(heap >= sets, "the count saw nothing: {heap} < {sets}");
    // a runtime tick lands in a window now and then, never in every one
    assert_eq!(inline.iter().min(), Some(&0), "steady-state commands allocate: {inline:?}");
    assert_eq!(long_gets.iter().min(), Some(&0), "long replies allocate: {long_gets:?}");
}
