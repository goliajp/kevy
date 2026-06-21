//! The cross-core drain + connection-reap half of the io_uring reactor.
//! Split out of [`crate::uring_reactor`] to keep that file under the
//! 500-LOC house rule — every method here is on the same
//! `impl<C: Commands> Shard<C>` and only ever called from `run_uring`.

use crate::Commands;
use crate::shard::Shard;
use crate::uring_reactor::UringConn;
use core::sync::atomic::Ordering;
use kevy_map::KevyMap;

impl<C: Commands> Shard<C> {
    /// Drain cross-core rings: execute forwarded requests, fold replies into
    /// their connection's output (no direct write — the io_uring arm/write
    /// loop flushes it). The message handling itself is
    /// [`Shard::drain_inbound_core_slow`], shared with the epoll reactor.
    ///
    /// **E15 (2026-06-20)** fast-path split: post-v1.24-chain perf
    /// diagnostic showed this at 3.59 % self — almost all from the per-iter
    /// fn call overhead despite the cheap Acquire load inside. Now the
    /// Acquire load lives here in a tiny `#[inline]` wrapper that LLVM
    /// folds into the reactor loop body; the cold drain body is
    /// `#[inline(never)]` so its bulk stays off the hot iTLB pages.
    #[inline]
    pub(crate) fn uring_drain_inbound(&mut self) -> bool {
        let me = self.id;
        if self.inbound_dirty[me].load(Ordering::Acquire) == 0 {
            return false;
        }
        self.drain_inbound_core_slow::<false>()
            .expect("DIRECT_FLUSH=false drain has no fallible step")
    }

    /// Close connections that are done: EOF/QUIT seen, all output flushed, no
    /// SQE in flight. Dropping the `Conn` closes the fd.
    ///
    /// E18 attempted a two-`any()`-scan fast-path bail (skip the Vec
    /// collect when no conn carries a closing flag) and reverted —
    /// at c100 the 2×N pre-scan added more cost than the avoided alloc
    /// saved (lx64 c100 SET -2.9 %), and the only sound way to use a
    /// single scan is to keep io.closing + conn.closing in sync (which
    /// requires plumbing the io map down into the dispatch QUIT path).
    /// Left for a future iteration that's willing to take that plumb.
    pub(crate) fn uring_reap_closed(&mut self, io: &mut KevyMap<u64, UringConn>) {
        let done: Vec<u64> = io
            .iter()
            .filter(|(cid, uc)| {
                let conn = self.conns.get(cid);
                let drained = conn.is_none_or(|c| {
                    c.output.is_empty() && c.pending.is_empty() && c.write_pos == 0
                });
                let closing = uc.closing || conn.is_some_and(|c| c.closing);
                // The multishot recv may still be armed; closing the fd (on Conn
                // drop) terminates it and its final completion is ignored (conn
                // gone). We only need writes fully flushed before closing.
                closing && !uc.write_inflight && uc.write_buf.is_empty() && drained
            })
            .map(|(&cid, _)| cid)
            .collect();
        for cid in done {
            // Use the shared teardown (not a local conns.remove): it also
            // cancels block waiters (local + cross-shard arbiter) and drops
            // pub/sub + pattern subscriptions. Skipping it leaked a parked
            // BLPOP/XREAD waiter and psub registrations on every io_uring
            // disconnect — a waiter left behind could consume a later push
            // meant for a live client. The epoll-only `poller.delete` /
            // `fd_to_conn` steps inside are harmless no-ops here (io_uring
            // never registered the fd with the readiness poller).
            self.close_conn(cid);
            io.remove(&cid);
            // Axis E follow-up: remove from the active list (swap_remove
            // keeps the list dense; order is irrelevant for arm_conns).
            if let Some(pos) = self.active_uring_conns.iter().position(|&c| c == cid) {
                self.active_uring_conns.swap_remove(pos);
            }
        }
    }
}
