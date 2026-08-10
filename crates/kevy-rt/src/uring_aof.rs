//! AOF offload (RFC v3-aof-offload S1): the io_uring reactor's side of
//! queued-append mode — take chunks off [`kevy_persist::Aof`]'s queue,
//! submit them as positioned `write` SQEs on the shard's own ring, and
//! keep every byte alive until its CQE. The reactor never traps into a
//! synchronous `write(2)` on the hot path, which is exactly the syscall
//! the Phase A decomposition convicted for the multi-second tail stalls
//! (dirty-page throttling parks the writer under GB/s ingest).
//!
//! S1 scope: `everysec` / `no` policies (replies never wait on the AOF);
//! `always` keeps today's synchronous path — its reply-gating moves in
//! S2. Off by default: `KEVY_AOF_OFFLOAD=1` opts in, and the epoll
//! reactor is untouched (S3's writer-thread lane).
//!
//! Ordering contract (the `queue` field's doc in kevy-persist): chunks
//! carry explicit non-overlapping offsets, so in-flight writes never
//! race each other's file position. Structural file operations
//! (rewrite begin/finish, truncate) must not run while a chunk is in
//! flight — [`Shard::uring_aof_restructure_ready`] gates them and the
//! tick retries once the ring drains (write CQEs are µs-scale).

#![cfg(target_os = "linux")]

use std::collections::VecDeque;
use std::time::Instant;

use crate::shard::Shard;
use crate::uring_ops::{OP_AOF, OP_SHIFT};
use crate::Commands;
use kevy_uring::IoUring;

/// One submitted-but-uncompleted append chunk. The bytes MUST outlive
/// the CQE (the SQE holds a raw pointer into them).
struct InflightChunk {
    seq: u64,
    offset: u64,
    bytes: Vec<u8>,
    /// Bytes already acknowledged by short-write CQEs; the remainder is
    /// resubmitted at `offset + written`.
    written: u32,
    /// The remainder still needs an SQE (fresh chunk, or a short write
    /// came back, or the SQ was full last attempt).
    needs_submit: bool,
}

/// Per-shard offload state. Lives on [`Shard`]; all methods are no-ops
/// unless [`Self::enabled`].
pub(crate) struct AofOffload {
    pub(crate) enabled: bool,
    inflight: VecDeque<InflightChunk>,
    next_seq: u64,
    fsync_inflight: bool,
    /// A write CQE completed since the last fsync completion.
    dirty_since_sync: bool,
    last_sync: Instant,
    /// A structural operation (BGREWRITEAOF, auto-rewrite) arrived while
    /// chunks were in flight; the tick retries it after the drain.
    pub(crate) want_restructure: bool,
}

impl Default for AofOffload {
    fn default() -> Self {
        Self {
            enabled: false,
            inflight: VecDeque::new(),
            next_seq: 1,
            fsync_inflight: false,
            dirty_since_sync: false,
            last_sync: Instant::now(),
            want_restructure: false,
        }
    }
}

impl<C: Commands> Shard<C> {
    /// Queued-append mode at reactor setup — DEFAULT ON for the
    /// io_uring reactor with an AOF and a non-`Always` policy (the
    /// whole S4/S5 arc: appends, fsyncs, tee, folds, swap and frees
    /// all off the reactor; tailgate green was measured in this mode).
    /// `KEVY_AOF_OFFLOAD=0/off/no/false` opts back into the classic
    /// synchronous path; `always` keeps it by definition (replies
    /// gate on the write).
    pub(crate) fn uring_aof_setup(&mut self) {
        let Some(aof) = &mut self.aof else { return };
        if matches!(
            std::env::var("KEVY_AOF_OFFLOAD").as_deref(),
            Ok("0" | "off" | "no" | "false")
        ) {
            return;
        }
        if matches!(aof.fsync_policy(), kevy_persist::Fsync::Always) {
            eprintln!(
                "kevy: shard {} AOF offload requested but appendfsync=always keeps the \
                 synchronous path in this build (S1 scope)",
                self.id
            );
            return;
        }
        aof.enable_queued_appends();
        self.aof_offload.enabled = true;
    }

    /// Per-iteration pump, replacing the synchronous `maybe_sync` when
    /// offload is on: submit queued chunks (and short-write remainders),
    /// then schedule the everysec fsync once the ring is empty of
    /// writes. When offload is off this falls back to the synchronous
    /// `maybe_sync` — byte-identical to the pre-offload reactor.
    pub(crate) fn uring_aof_tick(&mut self, ring: &mut IoUring) {
        if !self.aof_offload.enabled {
            if let Some(aof) = &mut self.aof {
                let _ = aof.maybe_sync();
            }
            return;
        }
        // New queue contents become an in-flight chunk.
        if let Some(aof) = &mut self.aof
            && let Some((offset, bytes)) = aof.take_pending()
        {
            let seq = self.aof_offload.next_seq;
            self.aof_offload.next_seq += 1;
            self.aof_offload.inflight.push_back(InflightChunk {
                seq,
                offset,
                bytes,
                written: 0,
                needs_submit: true,
            });
        }
        self.uring_aof_submit_pending(ring);
        self.uring_aof_maybe_fsync(ring);
    }

    /// Submit every chunk (or remainder) still waiting for an SQE. SQ
    /// full is fine — the flag stays set and the next iteration retries.
    fn uring_aof_submit_pending(&mut self, ring: &mut IoUring) {
        let Some(fd) = self.aof.as_ref().and_then(kevy_persist::Aof::queued_fd) else {
            return;
        };
        for c in self.aof_offload.inflight.iter_mut().filter(|c| c.needs_submit) {
            let rest = &c.bytes[c.written as usize..];
            let ud = OP_AOF | c.seq;
            // SAFETY: `rest` points into `c.bytes`, which lives in
            // `inflight` until this chunk's final CQE is reaped.
            let ok = unsafe {
                ring.prep_write_at(fd, rest.as_ptr(), rest.len() as u32, c.offset + u64::from(c.written), ud)
            };
            if !ok {
                return; // SQ full; retry next iter (order preserved: we stop at the first miss)
            }
            c.needs_submit = false;
        }
    }

    /// everysec: one DATASYNC SQE per elapsed window, submitted only
    /// when no write is in flight (ordering without a ring-wide drain —
    /// the fsync must not overtake a write it is meant to cover).
    fn uring_aof_maybe_fsync(&mut self, ring: &mut IoUring) {
        let o = &mut self.aof_offload;
        if o.fsync_inflight
            || !o.dirty_since_sync
            || !o.inflight.is_empty()
            || o.last_sync.elapsed().as_secs() < 1
        {
            return;
        }
        let Some(aof) = &mut self.aof else { return };
        if aof.swap_holding() || !matches!(aof.fsync_policy(), kevy_persist::Fsync::EverySec) {
            return;
        }
        let Some(fd) = aof.queued_fd() else { return };
        if ring.prep_fsync(fd, OP_AOF) {
            o.fsync_inflight = true;
        }
    }

    /// CQE for an offload SQE: `user_data` low bits carry the chunk seq
    /// (0 = the fsync). Errors mirror the synchronous path's
    /// best-effort contract (`Shard::log` eprintlns and carries on).
    pub(crate) fn uring_aof_on_cqe(&mut self, user_data: u64, res: i32) {
        let seq = user_data & !(0xF << OP_SHIFT);
        if seq == 0 {
            self.aof_offload.fsync_inflight = false;
            if res < 0 {
                eprintln!("kevy: shard {} aof offload fsync failed: errno {}", self.id, -res);
            } else {
                self.aof_offload.dirty_since_sync = false;
                self.aof_offload.last_sync = Instant::now();
            }
            return;
        }
        let Some(pos) = self.aof_offload.inflight.iter().position(|c| c.seq == seq) else {
            return; // already reaped (defensive)
        };
        if res < 0 {
            // Same contract as the synchronous append: report loudly,
            // do not tear the shard down. The bytes stay queued for a
            // retry next iteration.
            eprintln!("kevy: shard {} aof offload write failed: errno {}", self.id, -res);
            self.aof_offload.inflight[pos].needs_submit = true;
            return;
        }
        let c = &mut self.aof_offload.inflight[pos];
        c.written += res as u32;
        if (c.written as usize) < c.bytes.len() {
            c.needs_submit = true; // short write: resubmit the remainder
            return;
        }
        self.aof_offload.inflight.remove(pos);
        self.aof_offload.dirty_since_sync = true;
    }

    /// May a structural file operation (rewrite begin/finish, truncate)
    /// run right now? False while any chunk is in flight — the caller
    /// sets `want_restructure` and the tick retries after the drain.
    pub(crate) fn uring_aof_restructure_ready(&self) -> bool {
        !self.aof_offload.enabled
            || (self.aof_offload.inflight.is_empty()
                && self.aof.as_ref().is_none_or(kevy_persist::Aof::queued_is_empty))
    }

    /// Tick wrapper for the persistence trio under offload: structural
    /// work (applying a finished rewrite, starting a new one) only runs
    /// with the ring drained of append chunks; a deferred explicit
    /// BGREWRITEAOF fires here too.
    pub(crate) fn uring_tick_persist(&mut self) {
        // The tee overrun check is a safety valve and must run even
        // while the ring is busy: the structural gate below stays
        // closed for exactly as long as saturating ingest keeps the
        // queue non-empty — which is exactly when the tee grows
        // unchecked (measured: a single-key LPUSH storm concentrated
        // on one shard grew a GB tee with zero defers logged, because
        // the gated tick never ran the check). The check itself does
        // no structural file ops: the abort is memory-only and the
        // unlinks ship to the worker.
        self.check_tee_overrun();
        if self.uring_aof_restructure_ready() {
            if self.aof_offload.want_restructure {
                self.aof_offload.want_restructure = false;
                self.start_bg_rewrite();
            }
            self.tick_persist();
        }
        // else: chunks in flight — CQEs land µs later; the next tick
        // (≤100 ms) retries. Stats/gauges skip one beat, nothing more.
    }

    /// Reactor-exit drain: block until every in-flight chunk completes,
    /// then hand the file back to the synchronous path (a final
    /// `sync_now` runs in the shard's shutdown sequence).
    pub(crate) fn uring_aof_drain_exit(&mut self, ring: &mut IoUring) {
        while !self.aof_offload.inflight.is_empty() {
            self.uring_aof_submit_pending(ring);
            let _ = ring.submit_and_wait(1);
            let mut aof_cqes: Vec<(u64, i32)> = Vec::with_capacity(8);
            ring.for_each_completion(|c| {
                if c.user_data & (0xF << OP_SHIFT) == OP_AOF {
                    aof_cqes.push((c.user_data, c.res));
                }
                // Non-AOF completions at exit (conn I/O on a stopping
                // shard) are dropped with their conns — same as the
                // pre-offload shutdown behavior.
            });
            for (ud, res) in aof_cqes {
                self.uring_aof_on_cqe(ud, res);
            }
        }
    }
}
