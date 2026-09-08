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
    /// Fast-path split: a perf
    /// diagnostic showed this at 3.59 % self — almost all from the per-iter
    /// fn call overhead despite the cheap Acquire load inside. Now the
    /// Acquire load lives here in a tiny `#[inline]` wrapper that LLVM
    /// folds into the reactor loop body; the cold drain body is
    /// `#[inline(never)]` so its bulk stays off the hot iTLB pages.
    #[inline]
    pub(crate) fn uring_drain_inbound(&mut self) -> usize {
        let me = self.id;
        if self.inbound_dirty[me].load(Ordering::Acquire) == 0 {
            return 0;
        }
        self.drain_inbound_core_slow::<false>()
            .expect("DIRECT_FLUSH=false drain has no fallible step")
    }

    /// Close connections that are done: EOF/QUIT seen, all output flushed, no
    /// SQE in flight. Dropping the `Conn` closes the fd.
    ///
    /// An earlier attempt tried a two-`any()`-scan fast-path bail (skip
    /// the Vec collect when no conn carries a closing flag) and reverted —
    /// at c100 the 2×N pre-scan added more cost than the avoided alloc
    /// saved (measured -2.9 % on the bench box's c100 SET shape), and
    /// the only sound way to use a
    /// single scan is to keep io.closing + conn.closing in sync (which
    /// requires plumbing the io map down into the dispatch QUIT path).
    /// Left for a future iteration that's willing to take that plumb.
    // LOC-WAIVER: closing ready-set reap state machine (classify / requeue /
    // shared teardown) — io_uring-only path, unverifiable on darwin;
    // waived rather than split without a runnable test surface.
    pub(crate) fn uring_reap_closed(&mut self, io: &mut KevyMap<u64, UringConn>) {
        // Drain the closing ready-set instead of
        // walking the whole io map. perf-record-dwarf at c=10 000 -P 1
        // SET sustained showed the prior `io.iter().filter(...).map(
        // |(cid, _)| (cid, self.conns.get(cid))).collect::<Vec<u64>>()`
        // body at 36.74 % of CPU — pure O(N) scan + per-entry second
        // hash lookup into `self.conns`. With the ready-set populated
        // by `uring_mark_closing` + the QUIT dispatch sites, this is
        // O(closing) per reap pass — typically 0-few entries at any
        // moment.
        //
        // A conn that is not yet quiet goes back on the set's tail.
        let candidates: Vec<u64> = std::mem::take(&mut self.closing_uring_conns);
        let mut done: Vec<u64> = Vec::with_capacity(candidates.len());
        let mut requeue: Vec<u64> = Vec::new();
        for cid in candidates {
            // Already reaped (e.g. dedup on a doubly-pushed cid)?
            let Some(uc) = io.get(&cid) else { continue };
            let conn = self.conns.get(&cid);
            // Sanity: cid was pushed because something flipped closing — but
            // accept-fail / EOF races could land it without `closing == true`.
            // Skip non-closing rather than reap.
            if !(uc.closing || conn.is_some_and(|c| c.closing)) {
                continue;
            }
            if closing_conn_is_quiet(uc, conn) {
                done.push(cid);
            } else {
                requeue.push(cid);
            }
        }
        // Restore retries for the next reap pass.
        self.closing_uring_conns.append(&mut requeue);
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
            // No per-conn list to maintain. A stale
            // entry in `arm_pending` for `cid` is a no-op next iter
            // (the arm loop bails when both `conns.get_mut(&cid)` and
            // `io.get_mut(&cid)` return None).
        }
    }
}

/// Whether a closing conn is finished with the ring and can be torn
/// down: nothing of it still in flight, nothing of it still unsent.
///
/// The recv term is the one that was missing. [`Shard::uring_arm_conns`]
/// cancels a closing conn's multishot recv precisely so that `close(fd)`
/// sends a FIN, and its comment ends "the next reap closes cleanly" —
/// but the next reap did not look at `recv_armed`. A reap landing in the
/// window between the cancel being submitted and its terminal CQE
/// arriving tore the conn down with the multishot still armed; the
/// socket stayed pinned in the kernel, `close(fd)` sent nothing, and a
/// client the server had decided to disconnect waited on a live socket
/// forever.
///
/// Measured rather than reasoned. The query-buffer cell failed 5-7 times
/// in 100 on Linux/io_uring, and in every one of those the reactor's own
/// stall dump reported `conns=0` for all 121 heartbeats spanning the
/// client's 30-second wait: the conn was already fully reaped while the
/// client still saw the socket open. That rules out everything upstream
/// of the reap and leaves the teardown itself.
///
/// Waiting here is bounded. The arm loop re-issues the cancel on every
/// visit to a closing conn (idempotent — a redundant one returns
/// `-ENOENT`) and keeps closing conns queued, and `recv_armed` clears on
/// either that cancel's `-ECANCELED` or a multishot that stops
/// delivering. If it somehow did not clear, the conn stays in
/// `self.conns` and the stall dump names it — which is the failure worth
/// having, the alternative being the silent half-open leak this fixes.
fn closing_conn_is_quiet(uc: &UringConn, conn: Option<&crate::conn::Conn>) -> bool {
    let drained =
        conn.is_none_or(|c| c.output.is_empty() && c.pending.is_empty() && c.write_pos == 0);
    let writes_quiet = !uc.write_inflight && uc.write_buf.is_empty();
    let recv_quiet = !uc.recv_armed;
    writes_quiet && recv_quiet && drained
}

#[cfg(test)]
mod tests {
    use super::closing_conn_is_quiet;
    use crate::uring_conn::UringConn;

    /// A `None` conn is a conn already gone from `self.conns`, which the
    /// reap treats as drained — so these cases isolate the three terms
    /// that live on the `UringConn` side.
    #[test]
    fn a_fresh_conn_with_nothing_outstanding_is_quiet() {
        assert!(closing_conn_is_quiet(&UringConn::new(), None));
    }

    /// The term this function was extracted to add. An armed multishot
    /// recv pins the socket in the kernel, so reaping here closes the
    /// descriptor without a FIN ever reaching the client — the
    /// query-buffer disconnect that was decided and never landed.
    #[test]
    fn an_armed_recv_is_not_quiet() {
        let mut uc = UringConn::new();
        uc.recv_armed = true;
        assert!(!closing_conn_is_quiet(&uc, None), "reaped with the recv still armed");
    }

    #[test]
    fn a_write_in_flight_is_not_quiet() {
        let mut uc = UringConn::new();
        uc.write_inflight = true;
        assert!(!closing_conn_is_quiet(&uc, None));
    }

    #[test]
    fn unsent_bytes_in_write_buf_are_not_quiet() {
        let mut uc = UringConn::new();
        uc.write_buf.push(b'x');
        assert!(!closing_conn_is_quiet(&uc, None));
    }
}
