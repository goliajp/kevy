//! Per-completion I/O handlers for the io_uring reactor: recv pump (with
//! provided-buffer copy-out + dispatch), write progress, and the
//! mark-closing teardown helper. Split out of [`crate::uring_reactor`] so
//! that file stays under the 500-LOC house rule; every method here is on
//! the same `impl<C: Commands> Shard<C>` and is only ever called from
//! `run_uring`.

use crate::Commands;
use crate::shard::Shard;
use crate::uring_conn::UringConn;
/// Linux `errno`s referenced by [`Shard::uring_on_recv`]'s big-arg
/// cancel handling. `ECANCELED = 125` (kernel emits `-ECANCELED` on
/// the target's terminal CQE after a successful `IORING_OP_ASYNC_CANCEL`).
const ECANCELED: i32 = 125;
use kevy_map::KevyMap;
use kevy_uring::{Completion, ProvidedBufRing};

/// Threshold above which the tail `$<N>\r\n` header in a freshly-received
/// chunk triggers an explicit `Vec::reserve` on the conn-input buffer. Set
/// to the multishot recv slab size so big-arg ingress avoids the 0→16→32→
/// 48→64K realloc storm on cold conns (identified by decomposition).
const BIG_ARG_RESERVE_THRESHOLD: usize = 16 * 1024;

/// Scan the tail of `buf` for a `$<digits>\r\n` bulk header and, if found
/// for a body ≥ [`BIG_ARG_RESERVE_THRESHOLD`], call `Vec::reserve` so the
/// subsequent recv chunks in the same batch can land without realloc.
///
/// Caller-cheap: walks at most ~32 trailing bytes per invocation (the
/// header is always tiny). When there is no trailing `$<digits>\r\n`
/// (or the implied body is small or already fits in the current
/// capacity) the function returns without touching `buf`.
fn preallocate_for_big_arg_tail(buf: &mut Vec<u8>) {
    // Must end in CRLF for the header to be complete in this chunk.
    let n = buf.len();
    if n < 4 || buf[n - 2] != b'\r' || buf[n - 1] != b'\n' {
        return;
    }
    // Walk backwards from CRLF skipping ASCII digits; stop at `$`.
    let mut i = n - 2; // position of the trailing '\r'
    let digits_end = i;
    while i > 0 && buf[i - 1].is_ascii_digit() {
        i -= 1;
    }
    if i == digits_end || i == 0 || buf[i - 1] != b'$' {
        return;
    }
    // SAFETY: i..digits_end is an ASCII-digit slice, parse as usize.
    let mut bulk_len: usize = 0;
    for &b in &buf[i..digits_end] {
        // 20-digit cap (u64 max is 20 chars); bail to avoid overflow.
        if bulk_len > usize::MAX / 10 {
            return;
        }
        bulk_len = bulk_len * 10 + (b - b'0') as usize;
    }
    if bulk_len < BIG_ARG_RESERVE_THRESHOLD {
        return;
    }
    // A body larger than the protocol's bulk cap will be rejected by
    // the parser (`kevy_resp::MAX_BULK_LEN`), so don't pre-grow for
    // it — an unclamped reserve here is a remote multi-terabyte alloc
    // from a single forged `$<huge>\r\n` header.
    if bulk_len > kevy_resp::MAX_BULK_LEN {
        return;
    }
    // Reserve room for the body bytes plus the trailing `\r\n` (+ a small
    // pad for the next command's header in pipelined traffic).
    let need = bulk_len + 32;
    let have = buf.capacity() - buf.len();
    if need > have {
        buf.reserve(need - have);
    }
}

impl<C: Commands> Shard<C> {
    /// A multishot recv completed: dispatch every complete command parsed
    /// directly out of the kernel-picked buffer when possible (avoiding
    /// the pbuf→conn.input memcpy), fall back to append-then-parse when
    /// a prior partial frame is already buffered, recycle the slab, and
    /// re-arm if the SQE ended.
    ///
    /// Two decomposition-driven restructurings shape this path:
    /// - **parse-from-slab** when `conn.input` is empty, the parser
    ///   borrows directly from `pbuf.bytes(bid, n)` and only the unparsed
    ///   suffix (rare — only on a partial trailing frame) is copied into
    ///   `conn.input`. Eliminates the always-on pbuf→input memcpy on the
    ///   single-chunk hot path (10 K SET / GET arrive in one chunk).
    /// - **pre-grow** when a `$<N>\r\n` bulk header tails the
    ///   buffer with N ≥ slab size, reserve N+32 bytes up front so the
    ///   subsequent multishot recv chunks of the same big SET body land
    ///   without the 0→16→32→48→64K realloc storm on a cold connection.
    // LOC-WAIVER: hot recv-completion state machine (big-arg cancel /
    // CRLF skip / bigbulk routing / slab dispatch) — per-op critical
    // path, splitting risks codegen change.
    pub(crate) fn uring_on_recv(
        &mut self,
        cid: u64,
        c: &Completion,
        io: &mut KevyMap<u64, UringConn>,
        pbuf: &mut ProvidedBufRing,
    ) {
        // -ECANCELED (-125) on a multishot recv is ALWAYS
        // the big-arg cancel cycle's terminal CQE (kevy doesn't issue
        // recv cancels anywhere else). Route to the state-machine
        // handler regardless of whether `pending_big_arg` is still set
        // — the body may have completed via in-flight multishot CQEs
        // between cancel submission and ECANCELED arrival, in which
        // case the handler safely no-ops state and re-arms multishot.
        if c.res == -ECANCELED {
            if let Some(uc) = io.get_mut(&cid) {
                uc.recv_armed = false;
            }
            self.uring_on_big_arg_target_canceled(cid, io);
            return;
        }
        // The multishot SQE stops firing once a completion lacks F_MORE (error,
        // ENOBUFS, or EOF) — mark it for re-arming next loop.
        // Suppress the auto-rearm only while we're mid-big-arg-cancel
        // (so the state machine drives recv mode).
        let mut suppress_rearm = false;
        if let Some(uc) = io.get_mut(&cid)
            && let Some(state) = uc.pending_big_arg.as_ref()
            && matches!(
                state.as_ref(),
                crate::uring_conn::BigArgState::BareSetCancelling { .. }
                    | crate::uring_conn::BigArgState::BareSetReading { .. }
            )
        {
            suppress_rearm = true;
        }
        if !c.has_more() {
            if let Some(uc) = io.get_mut(&cid) {
                uc.recv_armed = false;
            }
            if !suppress_rearm {
                // Needs an arm visit to re-prep the recv SQE.
                self.mark_arm_pending(cid, io);
            }
        }
        if c.res <= 0 {
            // res == 0 is EOF ONLY with F_SOCK_NONEMPTY clear; with it
            // set the recv terminated but the socket still holds bytes
            // (see [`Self::recv_terminal_recoverable`]). The !has_more
            // block above already queued the re-arm; here, don't close.
            if !self.recv_terminal_recoverable(cid, c, io) {
                self.uring_mark_closing(cid, io);
            }
            return;
        }
        let Some(bid) = c.buffer_id() else {
            return; // no buffer (shouldn't happen for a successful recv)
        };
        let n = c.res as usize;
        // Data flowed: reset the zero-completion streak guard (folded
        // into the crlf-skip probe's get_mut — res > 0 here). The probe
        // slices any pending kernel-direct big-arg trailing CRLF off the
        // slab head before dispatch sees it.
        let n = if let Some(uc) = io.get_mut(&cid) && {
            uc.recv_zero_streak = 0;
            uc.pending_crlf_skip > 0
        } {
            let skip = (uc.pending_crlf_skip as usize).min(n);
            uc.pending_crlf_skip -= skip as u8;
            let slab_bytes = pbuf.bytes(bid, n);
            if skip > 0
                && let Some(uc) = io.get_mut(&cid)
                && uc.pending_big_arg.is_none()
            {
                // Verify and consume — if the leading bytes aren't
                // CRLF, protocol corruption: close.
                if !slab_bytes[..skip].iter().all(|b| matches!(*b, b'\r' | b'\n')) {
                    self.protocol_error(cid);
                    self.uring_mark_closing(cid, io);
                    pbuf.recycle(bid);
                    return;
                }
            }
            if skip == n {
                // Slab is entirely CRLF skip — nothing left to dispatch.
                pbuf.recycle(bid);
                self.mark_arm_pending(cid, io);
                return;
            }
            n - skip
        } else {
            n
        };
        // Recompute slab offset for the BigBulk routing below.
        let slab_offset = c.res as usize - n;
        // BigBulk routing: if this conn has a SET
        // value body in flight, feed slab bytes straight into the owned
        // dest Vec — ONE memcpy per chunk (slab → dest), same byte cost
        // as the prior slab→input path but the dest Vec is pre-sized
        // (no realloc storm).
        if let Some(uc) = io.get_mut(&cid)
            && uc.pending_big_arg.is_some()
        {
            self.aof_begin_group();
            let total = slab_offset + n;
            let slab_bytes = &pbuf.bytes(bid, total)[slab_offset..];
            self.uring_bigbulk_feed(cid, io, slab_bytes);
            pbuf.recycle(bid);
            self.aof_end_group_logged();
            // The feed may have completed the body and pushed +OK to
            // conn.output; queue the conn so the next arm visit
            // submits a write SQE.
            self.mark_arm_pending(cid, io);
            return;
        }
        // Take conn.input onto the stack so dispatch's borrowed argv
        // doesn't collide with `&mut self`. If the conn vanished between
        // the recv arming and the CQE (rare; close races), still need to
        // recycle the slab buffer to avoid starving the ring.
        let mut input_buf = match self.conns.get_mut(&cid) {
            Some(c) => std::mem::take(&mut c.input),
            None => {
                pbuf.recycle(bid);
                return;
            }
        };
        self.aof_begin_group();
        let total = slab_offset + n;
        let slab_for_dispatch = &pbuf.bytes(bid, total)[slab_offset..];
        let outcome = self.uring_recv_dispatch(cid, slab_for_dispatch, &mut input_buf, io);
        pbuf.recycle(bid);
        self.aof_end_group_logged();
        if outcome.conn_gone {
            return;
        }
        if let Some(c) = self.conns.get_mut(&cid) {
            c.input = input_buf;
        }
        if outcome.protocol_error {
            self.protocol_error(cid);
            self.uring_mark_closing(cid, io);
        }
        // Dispatch may have appended reply bytes to `conn.output` and/or
        // arc references to `conn.output_arcs` — queue the conn so the
        // next arm visit submits the write SQE. Cheap (one map probe +
        // a dedup flag) and unconditional: under bench-shape -P1 every
        // recv produces a reply, so the branch predictor stays hot.
        self.mark_arm_pending(cid, io);
    }

    /// Inner recv → parse → dispatch step. Picks the parse-from-slab fast
    /// path when `input_buf` is empty, otherwise appends + parses out of
    /// the combined buffer. AOF group-commit + slab recycle bookkeeping
    /// stays in [`Self::uring_on_recv`] (the caller).
    ///
    /// After the regular dispatch, the leftover
    /// (unparsed) tail is checked for a `SET key $<N>` BigBulk shape; if
    /// matched, the conn flips into BigBulk-recv mode (subsequent CQE
    /// bytes go straight into an owned dest Vec). This avoids both the
    /// `conn.input` realloc storm AND the final `Arc::from(slice)`
    /// 64K memcpy on big SETs.
    #[inline]
    // LOC-WAIVER: hot parse-from-slab dispatch fork (slab fast path vs
    // append-then-parse) — per-op critical path.
    pub(crate) fn uring_recv_dispatch(
        &mut self,
        cid: u64,
        slab: &[u8],
        input_buf: &mut Vec<u8>,
        io: &mut KevyMap<u64, UringConn>,
    ) -> crate::inbox::BatchOutcome {
        if input_buf.is_empty() {
            // Fast path: parse straight from the slab. The kernel's
            // provided-buffer slice lives until `pbuf.recycle(bid)`, which
            // the caller defers until after dispatch_batch returns. Any
            // bytes dispatch stores (e.g. `Arc::from(&[u8])` for SET) get
            // copied, so no slab byte escapes. Any unparsed suffix —
            // partial trailing frame mid-batch — is copied into
            // `input_buf` for the next CQE.
            let o = self.dispatch_batch(cid, slab);
            if !o.conn_gone && o.consumed < slab.len() {
                // Before staging the tail into
                // `input_buf` (where it would otherwise drive the
                // realloc storm for any subsequent body CQEs), probe
                // for the SET BigBulk shape. On a hit, promote: the
                // tail's body bytes (if any) go into the dest Vec; no
                // copy into `input_buf` at all.
                let tail = &slab[o.consumed..];
                if self.try_promote_bigbulk(cid, tail, io) {
                    return o;
                }
                input_buf.extend_from_slice(tail);
                preallocate_for_big_arg_tail(input_buf);
            }
            o
        } else {
            // Slow path: a prior partial frame already lives in
            // input_buf. Append + parse out of the combined buffer.
            // Triggers on multi-chunk frames (big SET ≥ slab size). The
            // pre-grow heuristic also applies after the append, so the
            // rest of the body lands without the realloc storm.
            input_buf.extend_from_slice(slab);
            preallocate_for_big_arg_tail(input_buf);
            let o = self.dispatch_batch(cid, input_buf);
            if !o.conn_gone {
                input_buf.drain(..o.consumed);
                // Probe the residue post-drain for a SET BigBulk shape.
                // If it matches, move the body bytes into the dest Vec
                // and CLEAR `input_buf` (the residue header bytes are
                // consumed by the probe; no need to keep them around).
                if !input_buf.is_empty() {
                    let promoted = {
                        let snapshot = std::mem::take(input_buf);
                        if self.try_promote_bigbulk(cid, &snapshot, io) {
                            true
                        } else {
                            *input_buf = snapshot;
                            false
                        }
                    };
                    if promoted {
                        return o;
                    }
                }
            }
            o
        }
    }

    /// Mark `cid` closing and eagerly cancel its block waiters (local
    /// parked BLPOP/XREAD + cross-shard arbiter registrations). The full
    /// teardown still happens in `uring_reap_closed`, but that runs on a
    /// 1/16-iteration throttle — without the eager cancel a dead conn's
    /// waiter stayed live for up to 16 iterations and could consume a
    /// push (e.g. an LPUSH element) meant for a live client.
    pub(crate) fn uring_mark_closing(&mut self, cid: u64, io: &mut KevyMap<u64, UringConn>) {
        if let Some(uc) = io.get_mut(&cid) {
            uc.closing = true;
        }
        // Closing conns stay in the arm queue until reap picks
        // them up — gives the arm loop a chance to drain any
        // outstanding write_buf before close_conn drops the fd.
        self.mark_arm_pending(cid, io);
        // Push to closing ready-set so
        // `uring_reap_closed` iterates O(closing) instead of O(N=conns).
        // Duplicates are harmless — the reap-side filter short-circuits
        // when self.conns.get(cid) returns None (already reaped).
        self.closing_uring_conns.push(cid);
        self.blocked.drop_for_conn(cid);
        self.cancel_xshard_on_close(cid);
    }

    /// io_uring twin of [`Self::enforce_output_limit`]: disconnect any
    /// conn whose pending `write_buf` (+ zero-copy arc bodies) has
    /// grown past [`crate::CLIENT_OUTPUT_HARD_LIMIT`], so a non-draining
    /// reader can't OOM the shard. Per-tick async sweep.
    pub(crate) fn uring_enforce_output_limit(&mut self, io: &mut KevyMap<u64, UringConn>) {
        let mut over: Vec<u64> = Vec::new();
        for (id, uc) in io.iter() {
            if uc.closing {
                continue;
            }
            let arc_bytes: usize = uc.write_arcs.iter().map(|(_, a)| a.len()).sum();
            if uc.write_buf.len().saturating_add(arc_bytes) > crate::CLIENT_OUTPUT_HARD_LIMIT {
                over.push(*id);
            }
        }
        for id in over {
            eprintln!(
                "kevy: shard {} closing conn {id}: output buffer exceeded {} bytes",
                self.id,
                crate::CLIENT_OUTPUT_HARD_LIMIT,
            );
            self.uring_mark_closing(id, io);
        }
    }

    /// A write completed: advance progress; resubmit the remainder next loop.
    // LOC-WAIVER: hot write-completion state machine (chunked-writev
    // prefix drop / short-write linearize) — per-op critical path.
    pub(crate) fn uring_on_write(
        &mut self,
        cid: u64,
        res: i32,
        io: &mut KevyMap<u64, UringConn>,
    ) {
        let Some(uc) = io.get_mut(&cid) else {
            return;
        };
        uc.write_inflight = false;
        if res < 0 {
            self.uring_mark_closing(cid, io);
            return;
        }
        // The writev path mixes write_buf
        // bytes with arc-bulk borrowed bytes via the iovec list.
        // Chunked writev: the SQE may cover only the leading
        // `arcs_in_flight` arcs + write_buf up through `write_byte_cap`;
        // remaining arcs / write_buf tail stay queued for the next
        // arm_conns iter. On a full completion we drop the processed
        // prefix; on a SHORT write we materialise EVERYTHING (in-flight
        // chunk's unsent suffix + all remaining arcs + remaining
        // write_buf tail) into a linear write_buf so the next iter
        // resumes via the plain `prep_write` path.
        if !uc.write_arcs.is_empty() {
            let written = res as usize;
            let submitted = uc.write_inflight_bytes;
            if written == submitted {
                // Full chunk completed. Drop the processed-prefix arcs;
                // advance write_off through the included header bytes.
                let consumed = uc.arcs_in_flight;
                let everything_done = consumed == uc.write_arcs.len()
                    && uc.write_byte_cap == uc.write_buf.len();
                if everything_done {
                    uc.write_buf.clear();
                    uc.write_arcs.clear();
                    uc.write_iovecs.clear();
                    uc.write_off = 0;
                    uc.arcs_in_flight = 0;
                    uc.write_byte_cap = 0;
                    uc.write_inflight_bytes = 0;
                    // H1.C: per-conn pending_write flag tracks the
                    // pub/sub dirty-list dedup. write_buf was swapped
                    // from conn.output earlier; once fully sent and
                    // conn.output is empty too, the conn is idle wrt
                    // outbound and the next publish should re-push it
                    // onto `dirty`.
                    if let Some(conn) = self.conns.get_mut(&cid)
                        && conn.output.is_empty()
                    {
                        conn.pending_write = false;
                    }
                } else {
                    // A.4: leave the unsent tail in place. write_off
                    // advances to the cap; the next arm_conns iter
                    // submits the next chunk starting from there.
                    uc.write_off = uc.write_byte_cap;
                    uc.write_arcs.drain(..consumed);
                    uc.write_iovecs.clear();
                    uc.arcs_in_flight = 0;
                    uc.write_byte_cap = 0;
                    uc.write_inflight_bytes = 0;
                }
            } else {
                // Short write: materialise the entire still-unsent
                // payload (in-flight chunk's unsent suffix + remaining
                // chunked-out arcs + write_buf tail past byte_cap) into
                // a linear write_buf; drop all arcs; reset chunked
                // state; advance write_off by the bytes actually
                // written. Next iter takes the simple prep_write path.
                let total: usize = uc.write_buf.len()
                    + uc.write_arcs.iter().map(|(_, a)| a.len()).sum::<usize>();
                let mut linear: Vec<u8> = Vec::with_capacity(total);
                let mut prev = 0usize;
                for (pos, arc) in &uc.write_arcs {
                    let pos = *pos;
                    if pos > prev {
                        linear.extend_from_slice(&uc.write_buf[prev..pos]);
                    }
                    linear.extend_from_slice(arc.as_ref());
                    prev = pos;
                }
                if prev < uc.write_buf.len() {
                    linear.extend_from_slice(&uc.write_buf[prev..]);
                }
                uc.write_buf = linear;
                uc.write_arcs.clear();
                uc.write_iovecs.clear();
                uc.write_off = written;
                uc.arcs_in_flight = 0;
                uc.write_byte_cap = 0;
                uc.write_inflight_bytes = 0;
            }
            return;
        }
        uc.write_off += res as usize;
        if uc.write_off >= uc.write_buf.len() {
            uc.write_buf.clear();
            uc.write_off = 0;
            // H1.C: see comment in the arc-write branch above.
            if let Some(conn) = self.conns.get_mut(&cid)
                && conn.output.is_empty()
            {
                conn.pending_write = false;
            }
        }
    }
}
