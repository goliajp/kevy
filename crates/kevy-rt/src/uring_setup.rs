//! io_uring setup face split from `uring_reactor.rs` (500-LOC rule):
//! ring capacities, the global availability probe, the per-shard
//! ring builder the spawn site uses for the auto-mode epoll fallback,
//! and the shard's pre-loop preparation.

use std::io;
use std::sync::Arc;

use kevy_persist::{load_snapshot, replay_aof};
use kevy_uring::IoUring;

use crate::Commands;
use crate::shard::Shard;

impl<C: Commands> Shard<C> {
    /// Everything `run_uring` does before its first loop iteration:
    /// the embedder's per-shard start hook (Lua / cross-shard registry
    /// setup lives behind it — dropping it broke EVAL across shards on
    /// the uring path only), the replica inbox's waker (see
    /// replica_inbox.rs's wake contract), and the snapshot + AOF
    /// restore, same as the readiness path.
    pub(crate) fn prepare_uring_shard(&mut self) -> io::Result<()> {
        self.commands.on_shard_start(self.id);
        if let Some(rx) = &self.replica_inbox {
            rx.attach_waker(Arc::clone(&self.waker));
        }
        let snap = self.snapshot_path();
        if snap.exists()
            && let Err(e) = load_snapshot(&mut self.store, &snap)
        {
            eprintln!("kevy: shard {} failed to load {}: {e}", self.id, snap.display());
        }
        if self.aof.is_some() {
            let aof_path = self.aof_path();
            let commands = &self.commands;
            let store = &mut self.store;
            let apply =
                |args: kevy_persist::Argv| crate::shard_run::replay_dispatch(commands, store, &args);
            let report = if self.replay_resync {
                kevy_persist::replay_aof_resync(&aof_path, apply)?
            } else {
                replay_aof(&aof_path, apply)?
            };
            self.commands.on_replay_report(report.dropped_bytes, report.corrupt);
        }
        Ok(())
    }
}

/// SQ/CQ depth per-shard. Paired with `PBUF_ENTRIES` — both were bumped
/// to fix the c=10 000 cliff (deco-axis-k-c10000).
pub(crate) const URING_ENTRIES: u32 = 2048;
// The nap rung was removed (see the idle-ladder comment in `run_uring`).
// URING_NAP_LIMIT / URING_NAP_MICROS / `uring_nap` are gone; spin →
// park is the whole ladder now.
/// Shared provided-buffer ring: 4096 × 16K = 64 MiB/shard. Linux multishot
/// recv terminates on ENOBUFS — must size for max conns (deco-axis-k-c10000).
pub(crate) const PBUF_ENTRIES: u16 = 4096;
pub(crate) const PBUF_SIZE: u32 = 16 * 1024;
pub(crate) const PBUF_GROUP: u16 = 0;

/// Probe whether this host can build the io_uring + provided-buffer ring that
/// [`Shard::run_uring`] needs: `io_uring_setup` not blocked by seccomp (Docker's
/// default profile blocks it) and a kernel new enough for the buf ring (5.19+).
/// Builds and immediately drops a real ring with the same parameters, so a
/// success here means `run_uring` will start. [`crate::Runtime`] calls this once
/// before spawning shards to auto-select io_uring with a graceful epoll fallback
/// — so an unavailable io_uring degrades to epoll instead of failing startup.
pub(crate) fn io_uring_available() -> bool {
    match IoUring::new(URING_ENTRIES) {
        Ok(ring) => ring
            .register_buf_ring(PBUF_ENTRIES, PBUF_SIZE, PBUF_GROUP)
            .is_ok(),
        Err(_) => false,
    }
}

/// Build the per-shard ring pair (SQ/CQ ring + provided-buffer ring).
///
/// Split out of [`Shard::run_uring`] so the spawn site can
/// attempt it BEFORE committing the shard to the io_uring path — a
/// per-shard setup failure (ENOMEM under memory pressure with many
/// shards, rlimit exhaustion) in auto mode then falls back to the
/// epoll reactor for that shard instead of killing its thread. The
/// global [`io_uring_available`] probe only proves ONE ring can be
/// built; N shards need N rings.
///
/// `KEVY_SQPOLL=1` opts the ring into kernel-side SQ polling
/// (`IORING_SETUP_SQPOLL`, idle 1000 ms). Measurement-only switch:
/// SQPOLL spawns one kernel poll thread per shard competing for the
/// same core set as the shard threads, so it loses badly on a fully
/// subscribed box — it exists so the A/B stays reproducible whenever
/// the tradeoff is re-judged (spare-core layouts, kernel changes).
pub(crate) fn build_uring() -> io::Result<(IoUring, kevy_uring::ProvidedBufRing)> {
    let sqpoll = matches!(
        std::env::var("KEVY_SQPOLL").ok().as_deref(),
        Some(v) if !v.is_empty() && v != "0" && v != "off" && v != "no" && v != "false"
    );
    let ring = if sqpoll {
        IoUring::new_sqpoll(URING_ENTRIES, 1000, None)?
    } else {
        IoUring::new(URING_ENTRIES)?
    };
    let pbuf = ring.register_buf_ring(PBUF_ENTRIES, PBUF_SIZE, PBUF_GROUP)?;
    Ok((ring, pbuf))
}
