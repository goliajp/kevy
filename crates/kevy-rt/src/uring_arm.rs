//! io_uring per-iter arm loop — submits a read for every idle open conn
//! and a write for every conn with pending output, reusing one fixed
//! buffer per direction per conn. Split out of [`crate::uring_reactor`]
//! so that file stays under the 500-LOC house rule; every method here is
//! on the same `impl<C: Commands> Shard<C>` and is only ever called
//! from `run_uring`.

use crate::Commands;
use crate::shard::Shard;
use crate::uring_conn::UringConn;
use crate::uring_reactor::{MAX_IOVECS_PER_WRITEV, OP_RECV, OP_WRITE};
use kevy_map::KevyMap;
use kevy_uring::IoUring;

impl<C: Commands> Shard<C> {
    /// **K4 (v1.25 A.9)**: schedule `cid` for the next `arm_conns` visit.
    /// Idempotent — `UringConn::arm_queued` dedupes pushes so a conn
    /// touched by recv + write + drain in the same iter only lands on
    /// the queue once. Safe to call when the conn was just dropped
    /// (the lookup misses and the call is a no-op).
    #[inline]
    pub(crate) fn mark_arm_pending(
        &mut self,
        cid: u64,
        io: &mut KevyMap<u64, UringConn>,
    ) {
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
    pub(crate) fn uring_arm_conns(
        &mut self,
        ring: &mut IoUring,
        io: &mut KevyMap<u64, UringConn>,
        bgid: u16,
    ) {
        // A3 (2026-06-20): prefetch UringConn ahead of the loop body.
        // H7 diagnostic showed L1D-miss stalls = 24.6% of total backend
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
        // **K4 (v1.25 A.9, 2026-06-22)**: iterate the dirty-set queue
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
            // Start a new write: move the conn's output (bytes + arc-bulk
            // references) into stable per-`UringConn` state.
            if !uc.write_inflight
                && uc.write_buf.is_empty()
                && uc.write_arcs.is_empty()
                && (!conn.output.is_empty() || !conn.output_arcs.is_empty())
            {
                std::mem::swap(&mut uc.write_buf, &mut conn.output);
                std::mem::swap(&mut uc.write_arcs, &mut conn.output_arcs);
                uc.write_off = 0;
            }
            // L1 (2026-06-21): if the write carries arc-bulk fragments, use
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
                    // **A.4 (v1.25)**: cap iovec count at
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
                        uc.write_iovecs.push(kevy_uring::Iovec {
                            iov_base: arc.as_ptr(),
                            iov_len: arc.len(),
                        });
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
                    uc.write_inflight_bytes =
                        uc.write_iovecs.iter().map(|v| v.iov_len).sum();
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
            // Arm a multishot recv if one isn't already running (it re-fires per
            // arrival into the shared provided-buffer ring, so this happens once
            // per connection, not once per read — the syscall-batching win).
            if !uc.recv_armed
                && !uc.closing
                && ring.prep_recv_multishot(conn.sock.raw(), bgid, OP_RECV | cid)
            {
                uc.recv_armed = true;
            }
            // K4: re-queue if more work remains. A chunked writev
            // capped the SQE before all arcs/tail bytes were covered;
            // the on_write completion handler will not have anything
            // to do until the next arm_conns iter submits the next
            // chunk. Closing conns must stay in the queue until reap
            // picks them up. Conns that successfully armed everything
            // (no inflight chunked-writev tail, recv armed, no fresh
            // output) drop out — the completion handlers and the
            // wake-up sites will re-queue them when there's work.
            let needs_more = uc.closing
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
