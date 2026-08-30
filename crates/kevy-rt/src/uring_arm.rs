//! io_uring per-iter arm loop — submits a read for every idle open conn
//! and a write for every conn with pending output, reusing one fixed
//! buffer per direction per conn. Split out of [`crate::uring_reactor`]
//! so that file stays under the 500-LOC house rule; every method here is
//! on the same `impl<C: Commands> Shard<C>` and is only ever called
//! from `run_uring`.

use crate::Commands;
use crate::shard::Shard;
use crate::uring_conn::UringConn;
use crate::uring_reactor::ENOBUFS;
use crate::uring_reactor::{MAX_IOVECS_PER_WRITEV, OP_RECV, OP_WRITE};
use kevy_map::KevyMap;
use kevy_uring::Completion;
use kevy_uring::IoUring;

impl<C: Commands> Shard<C> {
    /// A terminating recv completion (`res <= 0`): `true` = recoverable
    /// re-arm, not a close. `-ENOBUFS` and `res == 0` with
    /// `F_SOCK_NONEMPTY` (socket still holds bytes — NOT EOF) re-arm; a
    /// per-conn streak counter caps a kernel that re-posts the zero
    /// completion without draining so the reactor can't livelock.
    pub(crate) fn recv_terminal_recoverable(
        &mut self,
        cid: u64,
        c: &Completion,
        io: &mut KevyMap<u64, UringConn>,
    ) -> bool {
        const RECV_ZERO_STREAK_CAP: u16 = 256;
        if c.res != -ENOBUFS && !(c.res == 0 && c.sock_nonempty()) {
            return false;
        }
        let Some(uc) = io.get_mut(&cid) else { return false };
        uc.recv_zero_streak = uc.recv_zero_streak.saturating_add(1);
        uc.recv_zero_streak <= RECV_ZERO_STREAK_CAP
    }

    /// Schedule `cid` for the next `arm_conns` visit.
    /// Idempotent — `UringConn::arm_queued` dedupes pushes so a conn
    /// touched by recv + write + drain in the same iter only lands on
    /// the queue once. Safe to call when the conn was just dropped
    /// (the lookup misses and the call is a no-op).
    #[inline]
    pub(crate) fn mark_arm_pending(&mut self, cid: u64, io: &mut KevyMap<u64, UringConn>) {
        if let Some(uc) = io.get_mut(&cid)
            && !uc.arm_queued
        {
            uc.arm_queued = true;
            self.arm_pending.push(cid);
        }
    }

    /// Submit a read for every idle open conn and a write for every conn with
    /// pending output, reusing one fixed buffer per direction per conn.
    ///
    /// One pass over `conns` with one `io` probe per conn: this loop runs
    /// every reactor iteration, and the previous shape (a `keys()` snapshot
    /// Vec + 3-8 map probes per conn to appease the borrow checker) was the
    /// hottest block of `run_uring` self time on the 8-shard profile. `conns`
    /// and `io` are disjoint borrows (`io` lives on `run_uring`'s stack), so
    /// `iter_mut` needs no snapshot — nothing here inserts or removes.
    // Hot per-iter arm loop — prefetch-pipelined per-conn SQE state
    // machine (write/writev chunking + big-arg cancel/read + multishot
    // re-arm); the hottest block of run_uring self-time.
    // LOC-WAIVER: hot per-iter arm loop; splitting risks codegen change.
    pub(crate) fn uring_arm_conns(
        &mut self,
        ring: &mut IoUring,
        io: &mut KevyMap<u64, UringConn>,
        bgid: u16,
    ) {
        // Prefetch UringConn ahead of the loop body.
        // A perf diagnostic showed L1D-miss stalls = 24.6% of total backend
        // stalls at -c1; scatter from conn-map and io-map accesses are
        // candidates. The conns map's slot for the upcoming conn is
        // already L1-hot at the call site, but its corresponding
        // UringConn (separately allocated via KevyMap<u64, UringConn>)
        // typically lives in a different cache line. Prefetching it
        // hides the L1 fill behind the prior iter's prep_write/recv
        // SQE writes.
        //
        // At -c1 single-conn the loop runs once → prefetch is a no-op
        // (next conn doesn't exist). At higher conn counts the
        // hide-fill benefit grows with iteration depth.
        //
        // Iterate the dirty-set queue
        // `arm_pending` instead of the dense `active_uring_conns: Vec`.
        // The arm-loop's prior shape walked O(N) conns per iter (e.g.
        // 10k entries at c=10k), bailing on the ~99 % idle ones in
        // ~5 ns each but still ~50 µs/iter raw. The dirty-set shape
        // visits only conns that signalled they need arm work — recv
        // re-arm after multishot termination, fresh output from
        // dispatch / fold / publish, chunked-writev continuations,
        // closing conns waiting for write drain. arm_pending is
        // populated at:
        //   - accept handler (new conn, needs recv arm)
        //   - uring_on_recv (produced output AND/OR recv terminated)
        //   - uring_on_write (chunked writev has more to send)
        //   - drain_inbound (folded reply added to conn.output)
        //   - publish path (pubsub + pattern; reuses self.dirty
        //     which is now drained into arm_pending each iter)
        //   - blocked / xshard reply paths (already push self.dirty)
        //   - mark_closing (conn needs visit until reap)
        // Per-iter cost goes from O(N=10k) to O(active) — at c=10k
        // SET -P1 active is bounded by the SQ depth (2048) and the
        // batch each conn produces.
        //
        // Per-conn `arm_queued: bool` flag dedupes pushes (same shape
        // as `pending_write` for `self.dirty`).
        //
        // Re-push on still-needs-work: after processing a conn, if its
        // chunked-writev SQE was capped (write_byte_cap < write_buf.len()
        // OR arcs_in_flight < write_arcs.len()), or if it's closing
        // and writes still in flight, push it back so the next iter
        // visits it again.
        //
        // Fold any pub/sub-style `self.dirty` pushes into the arm
        // queue. Pubsub + xshard reply + blocked-waiter paths already
        // dedupe via pending_write; we just route them to the same
        // queue here.
        if !self.dirty.is_empty() {
            // Drain self.dirty into arm_pending. Dedup against
            // `arm_queued` (UringConn flag) — pubsub may have pushed a
            // conn that we just processed and re-queued in the same
            // iter (e.g. publish-then-recv-re-arm).
            while let Some(cid) = self.dirty.pop() {
                if let Some(uc) = io.get_mut(&cid)
                    && !uc.arm_queued
                {
                    uc.arm_queued = true;
                    self.arm_pending.push(cid);
                }
            }
        }
        if self.arm_pending.is_empty() {
            return;
        }
        // Swap out so we can re-push during processing without
        // disturbing the iteration. Reuses the Vec storage.
        let mut queue = std::mem::take(&mut self.arm_pending);
        // S2 Always gate threshold, copied out so the per-conn borrows
        // below stay disjoint. Advances only via fsync CQEs / structural
        // durability, both outside this loop.
        let durable = self.aof_offload.durable_watermark;
        let mut prev: Option<*const UringConn> = None;
        for &cid in &queue {
            let Some(conn) = self.conns.get_mut(&cid) else {
                // Conn dropped between queueing and visit; the
                // matching UringConn entry will be cleaned by the
                // reap path (which also tolerates a missing conn).
                if let Some(uc) = io.get_mut(&cid) {
                    uc.arm_queued = false;
                }
                prev = None;
                continue;
            };
            if let Some(p) = prev {
                // Hint to the CPU: the previous iter's UringConn was
                // here — bringing it in pre-emptively warms the line
                // for the next iter's get_mut hit-write. x86_64 has a
                // dedicated `_mm_prefetch` intrinsic; aarch64 has
                // `__pld` but exposing it via the unstable `prfm`
                // intrinsic would gate on nightly, so on non-x86_64
                // targets we skip the hint and rely on the natural
                // hardware prefetcher.
                // SAFETY: pointer was a valid &mut UringConn from the
                // previous iteration; KevyMap doesn't reallocate inside
                // this loop (no insert/remove).
                #[cfg(target_arch = "x86_64")]
                unsafe {
                    core::arch::x86_64::_mm_prefetch::<{ core::arch::x86_64::_MM_HINT_T0 }>(
                        p as *const i8,
                    );
                }
                let _ = p; // silence unused on non-x86_64
            }
            let Some(uc) = io.get_mut(&cid) else {
                prev = None;
                continue;
            };
            prev = Some(uc as *const UringConn);
            uc.arm_queued = false;
            // S2 Always gate: a write's reply bytes stay in
            // `conn.output` until the fsync CQE proves its records
            // durable. The needs_more check below sees the un-swapped
            // output and re-queues the conn, so release is re-checked
            // every pass (≤1 pass of latency after the CQE). Bytes
            // already in `write_buf` were released earlier and may
            // proceed regardless.
            let gate_open = match uc.held_watermark {
                Some(h) if h > durable => false,
                Some(_) => {
                    uc.held_watermark = None;
                    true
                }
                None => true,
            };
            // Start a new write: move the conn's output (bytes + arc-bulk
            // references) into stable per-`UringConn` state.
            if gate_open
                && !uc.write_inflight
                && uc.write_buf.is_empty()
                && uc.write_arcs.is_empty()
                && (!conn.output.is_empty() || !conn.output_arcs.is_empty())
            {
                std::mem::swap(&mut uc.write_buf, &mut conn.output);
                std::mem::swap(&mut uc.write_arcs, &mut conn.output_arcs);
                uc.write_off = 0;
            }
            // If the write carries arc-bulk fragments, use
            // `prep_writev` with an iovec list — header bytes from write_buf
            // and value bytes from the pinned Arc<[u8]> sources fuse into ONE
            // syscall and avoid the per-GET memcpy of the value into
            // write_buf. Otherwise the simple `prep_write` path (no
            // overhead).
            if !uc.write_inflight
                && (uc.write_off < uc.write_buf.len() || !uc.write_arcs.is_empty())
            {
                let ok = if uc.write_arcs.is_empty() {
                    // Simple linear path — no arc-bulks pinned. Same as
                    // before.
                    unsafe {
                        ring.prep_write(
                            conn.sock.raw(),
                            uc.write_buf.as_ptr().add(uc.write_off),
                            (uc.write_buf.len() - uc.write_off) as u32,
                            OP_WRITE | cid,
                        )
                    }
                } else {
                    // Build the iovec scratch: walk write_arcs sorted by
                    // position. For each (pos, arc) pair, emit:
                    //   1. write_buf[prev_pos..pos] (header / static bytes)
                    //   2. arc.as_ref()             (zero-copy value bytes)
                    // Then a final write_buf[last_pos..len()] tail. Start
                    // from write_off to honour any prior partial-write
                    // resume.
                    //
                    // Cap iovec count at
                    // [`MAX_IOVECS_PER_WRITEV`] (Linux `IOV_MAX = 1024`).
                    // A pipelined pub/sub burst (1024 publishes × 50
                    // subs) puts >2000 iovecs onto a single conn; we
                    // submit one chunk per arm_conns iter and let the
                    // CQE handler drop the processed prefix. Without
                    // the cap the kernel returns -EINVAL.
                    uc.write_iovecs.clear();
                    let mut prev = uc.write_off;
                    let mut arcs_consumed = 0usize;
                    let mut byte_cap = uc.write_buf.len();
                    for (i, (pos, arc)) in uc.write_arcs.iter().enumerate() {
                        let pos = *pos;
                        // We may push up to 2 iovecs this iter (a header
                        // gap before the arc + the arc itself). Reserve
                        // one slot for the trailing tail-after-last-arc
                        // entry so capped submissions still end on a
                        // contiguous byte boundary.
                        let need = if pos > prev { 2 } else { 1 };
                        if uc.write_iovecs.len() + need > MAX_IOVECS_PER_WRITEV - 1 {
                            // Submit through end of the LAST included arc
                            // (the previous iter): byte_cap = `prev`.
                            // arcs_consumed already captures the count.
                            byte_cap = prev;
                            break;
                        }
                        if pos > prev {
                            uc.write_iovecs.push(kevy_uring::Iovec {
                                iov_base: uc.write_buf.as_ptr().wrapping_add(prev),
                                iov_len: pos - prev,
                            });
                        }
                        uc.write_iovecs
                            .push(kevy_uring::Iovec { iov_base: arc.as_ptr(), iov_len: arc.len() });
                        prev = pos;
                        arcs_consumed = i + 1;
                    }
                    if prev < byte_cap {
                        uc.write_iovecs.push(kevy_uring::Iovec {
                            iov_base: uc.write_buf.as_ptr().wrapping_add(prev),
                            iov_len: byte_cap - prev,
                        });
                    }
                    uc.arcs_in_flight = arcs_consumed;
                    uc.write_byte_cap = byte_cap;
                    uc.write_inflight_bytes = uc.write_iovecs.iter().map(|v| v.iov_len).sum();
                    // SAFETY: write_buf, write_arcs (Arc keeps bytes
                    // alive), and write_iovecs all live in `uc`, which
                    // is in the io map — they outlive any SQE we submit
                    // before reaping its CQE. The Iovec ptrs reference
                    // those memories.
                    unsafe {
                        ring.prep_writev(
                            conn.sock.raw(),
                            uc.write_iovecs.as_ptr(),
                            uc.write_iovecs.len() as u32,
                            OP_WRITE | cid,
                        )
                    }
                };
                if ok {
                    uc.write_inflight = true;
                }
            }
            // Three SQE submissions for the big-arg
            // cancel / single-shot read / re-arm cycle. Each gated on a
            // per-conn flag set by the state machine.
            if uc.big_arg_cancel_pending {
                // Belt-and-braces: only cancel if the state still wants
                // it. The body-completed-via-multishot race in
                // `uring_bigbulk_feed::BareSetCancelling` clears this
                // flag pre-arm-pass, but if a future code path
                // accidentally leaves it set when the state has been
                // taken, we'd cancel the freshly re-armed multishot
                // and wedge the conn.
                let still_cancelling = matches!(
                    uc.pending_big_arg.as_deref(),
                    Some(crate::uring_conn::BigArgState::BareSetCancelling { .. })
                );
                if !still_cancelling {
                    uc.big_arg_cancel_pending = false;
                } else {
                    let target = OP_RECV | cid;
                    let user_data = crate::uring_reactor::OP_BIG_CANCEL | cid;
                    if ring.prep_cancel(target, user_data) {
                        uc.big_arg_cancel_pending = false;
                    }
                }
            }
            if uc.big_arg_read_pending && !uc.closing {
                if let Some(boxed) = uc.pending_big_arg.as_mut()
                    && let crate::uring_conn::BigArgState::BareSetReading { body, body_len, .. } =
                        boxed.as_mut()
                {
                    // prep_read SQE length = remaining body bytes.
                    // The trailing CRLF is consumed via the re-armed
                    // multishot after dispatch (slab head is checked
                    // for the 2 leading CRLF bytes and skipped in
                    // `uring_on_recv` / `uring_recv_dispatch`).
                    let body_remaining = *body_len - body.len();
                    if body_remaining > 0 {
                        let read_user_data = crate::uring_reactor::OP_BIG_READ | cid;
                        // SAFETY: body Vec capacity is exactly
                        // `body_len`; pointer is valid for writes up to
                        // `body_remaining` bytes.
                        let ptr = unsafe { body.as_mut_ptr().add(body.len()) };
                        let ok = unsafe {
                            ring.prep_read(
                                conn.sock.raw(),
                                ptr,
                                body_remaining as u32,
                                read_user_data,
                            )
                        };
                        if ok {
                            uc.big_arg_read_pending = false;
                        }
                    } else {
                        uc.big_arg_read_pending = false;
                    }
                } else {
                    // State went away (completed via multishot before
                    // arm pass) — clear the flag.
                    uc.big_arg_read_pending = false;
                }
            }
            // Re-arm multishot recv:
            //  (a) default — when nothing else is gating it.
            //  (b) big-arg path — after big-arg completion, when
            //      `big_arg_rearm_recv` is set.
            // Both paths converge on the same `prep_recv_multishot` call.
            //
            // Only the BareSet cancel/read cycle OWNS recv mode (it
            // cancels the multishot and reads the body itself). The
            // `Frame` variant — cross-shard bare-SET, SETEX/APPEND/MSET,
            // the common path on a multi-shard instance — stitches its
            // bytes from the ORDINARY multishot, so gating the re-arm on
            // `pending_big_arg.is_none()` wedged it: when the multishot
            // ended on its own (ENOBUFS on the buffer ring is routine
            // under a deep pipeline), nothing re-armed it, the frame
            // never completed, and with no pending SQE and no output the
            // conn dropped out of the arm queue for good. Captured:
            // `big_arg=Frame(3232/4132) recv_armed=false arm_queued=false`.
            // `uring_on_recv`'s `suppress_rearm` already made exactly
            // this distinction — the two sites were inconsistent.
            let big_arg_owns_recv = matches!(
                uc.pending_big_arg.as_deref(),
                Some(
                    crate::uring_conn::BigArgState::BareSetCancelling { .. }
                        | crate::uring_conn::BigArgState::BareSetReading { .. }
                )
            );
            let want_multishot = !uc.recv_armed && !uc.closing && !big_arg_owns_recv;
            let recv_arm_wanted =
                (want_multishot || uc.big_arg_rearm_recv) && !uc.recv_armed && !uc.closing;
            let recv_armed_now =
                recv_arm_wanted && ring.prep_recv_multishot(conn.sock.raw(), bgid, OP_RECV | cid);
            if recv_armed_now {
                uc.recv_armed = true;
                uc.big_arg_rearm_recv = false;
            }
            // A closing conn whose multishot recv is still armed keeps
            // the socket pinned in the kernel — `close(fd)` at reap
            // then never sends FIN and the conn leaks half-open (the
            // query-buffer / output guard's disconnect never reaches
            // the client). Cancel the recv; its terminal CQE clears
            // `recv_armed`, and the next reap closes cleanly. Skip when
            // the big-arg state machine owns the recv (it runs its own
            // cancel). Idempotent: a redundant cancel returns -ENOENT.
            if uc.closing && uc.recv_armed && !big_arg_owns_recv {
                ring.prep_cancel(OP_RECV | cid, crate::uring_reactor::OP_BIG_CANCEL | cid);
            }
            // A wanted recv-arm the SQ couldn't take THIS iter (ring
            // momentarily full — a burst of pub/sub fan-out writes or
            // many conns arming at once) must NOT let the conn drop out
            // of the queue: with no armed recv SQE and no pending
            // output to re-trigger it, the connection wedges forever
            // (client blocked on a reply for a request the server
            // never reads — observed as a conn stuck at `cmd=NULL
            // events=r` while the reactor busy-loops). Keep it queued
            // to retry next iter, once submit drains the SQ.
            let recv_arm_deferred = recv_arm_wanted && !recv_armed_now;
            // Re-queue if more work remains. A chunked writev
            // capped the SQE before all arcs/tail bytes were covered;
            // the on_write completion handler will not have anything
            // to do until the next arm_conns iter submits the next
            // chunk. Closing conns must stay in the queue until reap
            // picks them up. Conns that successfully armed everything
            // (no inflight chunked-writev tail, recv armed, no fresh
            // output) drop out — the completion handlers and the
            // wake-up sites will re-queue them when there's work.
            // A big-arg cancel / single-shot read the SQ couldn't take
            // THIS iter is the same trap as `recv_arm_deferred`: the flag
            // stays set (correct — it retries), but nothing else re-queues
            // the conn, because the CQE that would is for the SQE we just
            // failed to submit. Under a deep pipeline of big-arg SETs the
            // ring fills routinely, so this wedged the conn for good
            // (captured: big_arg=true recv_armed=false arm_queued=false
            // in_arm_pending=false, output/write empty). Both flags are
            // cleared on successful submission or when their state goes
            // away, so keeping the conn queued always terminates.
            let big_arg_submit_deferred = uc.big_arg_cancel_pending || uc.big_arg_read_pending;
            let needs_more = uc.closing
                || recv_arm_deferred
                || big_arg_submit_deferred
                || (!uc.write_inflight
                    && (uc.write_off < uc.write_buf.len() || !uc.write_arcs.is_empty()))
                || (!conn.output.is_empty() || !conn.output_arcs.is_empty());
            if needs_more && !uc.arm_queued {
                uc.arm_queued = true;
                self.arm_pending.push(cid);
            }
        }
        // Reuse the queue Vec's storage for the next iter — avoid the
        // alloc churn of `Vec::new()`.
        queue.clear();
        if self.arm_pending.is_empty() {
            self.arm_pending = queue;
        }
    }
}
