//! One shard thread: pick the reactor, run it, and on the way out let the
//! restore gate know this shard will not hold the others back. Split from
//! `runtime_run.rs` for the 500-LOC house rule.

use crate::Commands;
use crate::shard::Shard;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Reactor selection on Linux:
///   KEVY_IO_URING unset → auto: try io_uring, fall back to epoll if the
///     host can't build the ring (probe below) — startup never fails.
///   KEVY_IO_URING=0/off/no/false → force the epoll readiness reactor.
///   KEVY_IO_URING=<anything else> → force io_uring (no fallback; a
///     setup failure then surfaces loudly — for benchmarks / tests).
/// The probe creates+drops a real ring with the run_uring parameters, so
/// it catches a seccomp-blocked io_uring_setup (Docker's default profile)
/// and pre-5.19 kernels before any shard loads data. (macOS = kqueue.)
#[cfg(target_os = "linux")]
pub(crate) fn reactor_choice(recv_buffers: u16) -> (bool, bool) {
    match std::env::var("KEVY_IO_URING").ok().as_deref() {
        Some("0") | Some("off") | Some("no") | Some("false") => (false, true),
        Some(_) => (true, true),
        None => {
            let avail = crate::uring_reactor::io_uring_available(recv_buffers);
            eprintln!(
                "kevy: reactor = {} (io_uring {})",
                if avail { "io_uring" } else { "epoll" },
                if avail {
                    "available"
                } else {
                    "unavailable — kernel <5.19 or seccomp; using epoll"
                },
            );
            (avail, false)
        }
    }
}

/// Non-Linux: always the readiness reactor (kqueue on macOS).
#[cfg(not(target_os = "linux"))]
pub(crate) fn reactor_choice(_recv_buffers: u16) -> (bool, bool) {
    (false, false)
}

/// One shard thread's body: pick the reactor and run it to completion.
///
/// Per-shard ring setup is attempted BEFORE committing to the
/// io_uring path. The global probe proves one ring builds; N shards
/// need N rings, and a late failure (ENOMEM under pressure) used to
/// kill the shard thread and leave a half-dead server (found via
/// GH-runner CI: blocking_cross_shard hangs). Auto mode now degrades
/// that shard to epoll, loudly. A forced KEVY_IO_URING=1 keeps the
/// old fail-loud contract.
pub(crate) fn run_shard_thread<C: Commands>(
    shard: Shard<C>,
    stop: Arc<AtomicBool>,
    use_uring: bool,
    uring_forced: bool,
) -> io::Result<()> {
    let (id, gate, halt) = (shard.id, Arc::clone(&shard.restore_gate), Arc::clone(&stop));
    #[cfg(target_os = "linux")]
    let res = if use_uring {
        match crate::uring_reactor::build_uring(shard.recv_buffers) {
            Ok(pair) => shard.run_uring(pair, stop),
            Err(e) if !uring_forced => {
                eprintln!(
                    "kevy: shard {id}: io_uring setup failed ({e}); \
                     falling back to the epoll reactor for this shard"
                );
                shard.run(stop)
            }
            Err(e) => Err(e),
        }
    } else {
        shard.run(stop)
    };
    #[cfg(not(target_os = "linux"))]
    let res = {
        let _ = (use_uring, uring_forced, shard.recv_buffers);
        shard.run(stop)
    };
    if let Err(e) = &res {
        eprintln!("kevy: shard {id} exited with error: {e}");
        // a server missing a shard would serve part of its keyspace
        halt.store(true, Ordering::Relaxed);
    }
    // a shard that stopped before it restored must not hold the others
    gate.arrive(id);
    res
}
