//! Ports for tests, and the wait that proves a server took one.
//!
//! Two CI failures on the 5.2.0 release branch, in two different tests,
//! came from the same place. Forty-two test files each carried their own
//! copy of:
//!
//! ```ignore
//! fn free_port() -> u16 {
//!     std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
//! }
//! ```
//!
//! That asks the kernel for a free ephemeral port, reads it, and **closes
//! the listener before the server binds it**. Run one test binary alone
//! and nothing races. Run the workspace under `cargo test`, where dozens
//! of processes draw from the same ephemeral range at once, and the
//! window is real — with two sides:
//!
//! * another test's server binds the port first, and the connection
//!   lands on a *different* engine. It answers correctly for its own
//!   state, so the test sees a well-formed reply about the wrong data:
//!   `RENAMENX` came back `:1` where this test's keys would have given
//!   `:0`.
//! * or the squatter has already exited, our server fails to bind, and
//!   the connect gets `Connection refused`.
//!
//! [`free_port`] narrows the window: ports come from a block this process
//! alone draws from, handed out by a counter that never repeats, and each
//! is confirmed bindable at the moment it is returned. Two processes
//! collide only if their block bases coincide *and* their counters line
//! up at the same instant.
//!
//! The window cannot be closed entirely from here — the server does the
//! binding, and a port cannot be held for it — so [`wait_listening`] and
//! [`assert_listening`] close the other half: they make a server that did
//! not come up **say so**, where the loops they replace exhausted their
//! attempts and carried on. That is the more important half. A silent
//! failure to bind is what turned a port collision into a test asserting
//! against someone else's data.

use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{Duration, Instant};

/// Ports per process block. Wide enough that a test binary never wraps
/// into a neighbour's block during one run.
const BLOCK: u16 = 64;
/// First port of the first block. Above the registered range and below
/// the ephemeral range Linux hands out by default (32768+), so this
/// scheme and the kernel's own allocator never draw from the same pool.
const FLOOR: u16 = 20_000;
/// How many blocks the space is divided into. `FLOOR + BLOCKS * BLOCK`
/// must stay under 32768.
const BLOCKS: u16 = 190;

/// The listener that proves this block is ours, held for the life of the
/// process, and the block's base.
///
/// A first version derived the base from `pid % BLOCKS` and stopped
/// there. That is not exclusion, it is a hope: with dozens of test
/// binaries running at once, two processes land on the same block often
/// — and when they do they collide *systematically*, because both walk
/// the block from offset zero in the same order. It was worse than the
/// ephemeral allocator it replaced, and the workspace suite said so.
///
/// Binding the base port and never letting go turns the block into a
/// claim: a second process trying the same block fails to bind and moves
/// to the next one. The anchor is never handed out.
static ANCHOR: std::sync::OnceLock<(TcpListener, u16)> = std::sync::OnceLock::new();

fn block_base() -> u16 {
    ANCHOR
        .get_or_init(|| {
            // Start where the pid points so processes spread out, then
            // walk — the walk is what makes it correct, the pid only
            // makes the first guess usually right.
            let start = std::process::id() as u16 % BLOCKS;
            for i in 0..BLOCKS {
                let base = FLOOR + ((start + i) % BLOCKS) * BLOCK;
                if let Ok(l) = TcpListener::bind(("127.0.0.1", base)) {
                    return (l, base);
                }
            }
            panic!(
                "kevy-testnet: every one of the {BLOCKS} port blocks in \
                 {FLOOR}..{} is claimed. Something is leaking test processes.",
                FLOOR + BLOCKS * BLOCK
            )
        })
        .1
}

/// Next offset within this process's block. Shared by [`free_port`] and
/// [`free_port_block`] so a run reserved by one is never handed out a
/// port at a time by the other — they draw from the same block.
static NEXT: AtomicU16 = AtomicU16::new(1); // 0 is the anchor

/// A port for this process to give to a server it is about to start.
///
/// Comes from a block this process holds exclusively, so no other test
/// binary will hand out the same number. It is still not *reserved* — the
/// server does the binding, and the moment between this returning and
/// that happening belongs to nobody — so pair it with [`assert_listening`]
/// and a lost race becomes a clear failure instead of a strange one.
pub fn free_port() -> u16 {
    let base = block_base();
    for _ in 0..BLOCK * 4 {
        let off = NEXT.fetch_add(1, Ordering::Relaxed) % BLOCK;
        if off == 0 {
            continue; // never the anchor
        }
        let p = base + off;
        if TcpListener::bind(("127.0.0.1", p)).is_ok() {
            return p;
        }
    }
    panic!("kevy-testnet: no free port in this process's block {base}..{}", base + BLOCK)
}

/// Wait until something accepts on `port`. `true` if it did.
pub fn wait_listening(port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// Wait until something accepts on `port`, or panic naming `what`.
///
/// The loops this replaces polled a fixed number of times and then fell
/// through whether or not anything had answered, so "the server started"
/// and "the server never bound" left by the same door. The failure then
/// surfaced later, somewhere else, as a connection refused or — worse —
/// as an assertion about another server's data.
pub fn assert_listening(port: u16, what: &str) {
    if !wait_listening(port, Duration::from_secs(10)) {
        panic!(
            "kevy-testnet: {what} never accepted on 127.0.0.1:{port} within 10s. \
             Either it failed to start, or another process took the port between \
             free_port() handing it out and {what} binding it."
        );
    }
}

/// `n` ports, all from this process's block, all distinct.
///
/// The local versions this replaces bound `n` listeners at once and
/// dropped them together, which widens the window rather than closing it:
/// every one of the `n` is exposed from the moment it is read until the
/// last server binds.
pub fn free_ports(n: usize) -> Vec<u16> {
    (0..n).map(|_| free_port()).collect()
}

/// The base of `width` consecutive free ports, all from this process's
/// block. Callers that need `base + i` for a small `i` — a server and its
/// replica, a cluster of shards — need them adjacent, not merely distinct.
///
/// Panics if `width` exceeds the block, which is a caller asking for more
/// than this scheme can promise rather than a transient failure.
pub fn free_port_block(width: usize) -> u16 {
    assert!(
        width < BLOCK as usize,
        "kevy-testnet: asked for a base plus {width} ports; a process block is {BLOCK}"
    );
    let base = block_base();
    // The contract is the one the callers were written against: the port
    // returned is free, AND so are the `width` ports after it. They use
    // `base + 1 ..= base + width` for the nodes and `base` for something
    // else, so reserving only `width` from `base` — which is what a first
    // version did — hands out a run whose tail was never checked. With
    // `width` of 0 it checked nothing at all and returned regardless.
    let run = width as u16 + 1;
    for _ in 0..BLOCK {
        let start = NEXT.fetch_add(run, Ordering::Relaxed) % BLOCK;
        if start == 0 || start + run > BLOCK {
            continue; // never the anchor, never past the block
        }
        let candidate = base + start;
        // Held together: a run is usable only if every port in it is free
        // at the same moment, which checking them one at a time does not
        // establish.
        let held: Vec<_> = (0..run)
            .map_while(|i| TcpListener::bind(("127.0.0.1", candidate + i)).ok())
            .collect();
        if held.len() == run as usize {
            return candidate;
        }
    }
    panic!("kevy-testnet: no run of {run} free ports in this process's block")
}
