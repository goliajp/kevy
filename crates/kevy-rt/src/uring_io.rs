//! Per-completion I/O handlers for the io_uring reactor: recv pump (with
//! provided-buffer copy-out + dispatch), write progress, and the
//! mark-closing teardown helper. Split out of [`crate::uring_reactor`] so
//! that file stays under the 500-LOC house rule; every method here is on
//! the same `impl<C: Commands> Shard<C>` and is only ever called from
//! `run_uring`.

use crate::Commands;
use crate::shard::Shard;
use crate::uring_conn::UringConn;
use crate::uring_reactor::ENOBUFS;
use kevy_map::KevyMap;
use kevy_uring::{Completion, ProvidedBufRing};

/// Threshold above which the tail `$<N>\r\n` header in a freshly-received
/// chunk triggers an explicit `Vec::reserve` on the conn-input buffer. Set
/// to the multishot recv slab size so big-arg ingress avoids the 0→16→32→
/// 48→64K realloc storm on cold conns (Axis B / v1.25 deco B-A3).
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
    /// **v1.25 deco G2 (Axis I + B)** restructures this path:
    /// - **A1 (parse-from-slab)** when `conn.input` is empty, the parser
    ///   borrows directly from `pbuf.bytes(bid, n)` and only the unparsed
    ///   suffix (rare — only on a partial trailing frame) is copied into
    ///   `conn.input`. Eliminates the always-on pbuf→input memcpy on the
    ///   single-chunk hot path (10 K SET / GET arrive in one chunk).
    /// - **B-A3 (pre-grow)** when a `$<N>\r\n` bulk header tails the
    ///   buffer with N ≥ slab size, reserve N+32 bytes up front so the
    ///   subsequent multishot recv chunks of the same big SET body land
    ///   without the 0→16→32→48→64K realloc storm on a cold connection.
    pub(crate) fn uring_on_recv(
        &mut self,
        cid: u64,
        c: &Completion,
        io: &mut KevyMap<u64, UringConn>,
        pbuf: &mut ProvidedBufRing,
    ) {
        // The multishot SQE stops firing once a completion lacks F_MORE (error,
        // ENOBUFS, or EOF) — mark it for re-arming next loop.
        if !c.has_more()
            && let Some(uc) = io.get_mut(&cid)
        {
            uc.recv_armed = false;
        }
        if c.res <= 0 {
            // Close on EOF (0) or a real error, but NOT on -ENOBUFS (the ring was
            // momentarily empty; the data is still queued, so just re-arm).
            if c.res != -ENOBUFS {
                self.uring_mark_closing(cid, io);
            }
            return;
        }
        let Some(bid) = c.buffer_id() else {
            return; // no buffer (shouldn't happen for a successful recv)
        };
        let n = c.res as usize;
        // **v1.25 B.4 + A.2** BigBulk routing: if this conn has a SET
        // value body in flight, feed slab bytes straight into the owned
        // dest Vec — ONE memcpy per chunk (slab → dest), same byte cost
        // as the prior slab→input path but the dest Vec is pre-sized
        // (no realloc storm) AND becomes the Arc<[u8]> body zero-copy
        // at completion (eliminating the final `Arc::from(&[u8])`
        // 64K memcpy). The slab can be recycled the moment its bytes
        // are appended — no need for an intermediate owned copy.
        if let Some(uc) = io.get_mut(&cid)
            && uc.pending_big_arg.is_some()
        {
            self.aof_begin_group();
            let slab_bytes = pbuf.bytes(bid, n);
            self.uring_bigbulk_feed(cid, io, slab_bytes);
            pbuf.recycle(bid);
            self.aof_end_group_logged();
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
        let outcome = self.uring_recv_dispatch(cid, pbuf.bytes(bid, n), &mut input_buf, io);
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
    }

    /// Inner recv → parse → dispatch step. Picks the parse-from-slab fast
    /// path when `input_buf` is empty, otherwise appends + parses out of
    /// the combined buffer. AOF group-commit + slab recycle bookkeeping
    /// stays in [`Self::uring_on_recv`] (the caller).
    ///
    /// **v1.25 B.4 + A.2** — after the regular dispatch, the leftover
    /// (unparsed) tail is checked for a `SET key $<N>` BigBulk shape; if
    /// matched, the conn flips into BigBulk-recv mode (subsequent CQE
    /// bytes go straight into an owned dest Vec). This avoids both the
    /// `conn.input` realloc storm AND the final `Arc::from(slice)`
    /// 64K memcpy on big SETs.
    #[inline]
    pub(crate) fn uring_recv_dispatch(
        &mut self,
        cid: u64,
        slab: &[u8],
        input_buf: &mut Vec<u8>,
        io: &mut KevyMap<u64, UringConn>,
    ) -> crate::inbox::BatchOutcome {
        let o = if input_buf.is_empty() {
            // A1 fast path: parse straight from the slab. The kernel's
            // provided-buffer slice lives until `pbuf.recycle(bid)`, which
            // the caller defers until after dispatch_batch returns. Any
            // bytes dispatch stores (e.g. `Arc::from(&[u8])` for SET) get
            // copied, so no slab byte escapes. Any unparsed suffix —
            // partial trailing frame mid-batch — is copied into
            // `input_buf` for the next CQE.
            let o = self.dispatch_batch(cid, slab);
            if !o.conn_gone && o.consumed < slab.len() {
                // **v1.25 B.4 + A.2** — before staging the tail into
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
        };
        o
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
        self.blocked.drop_for_conn(cid);
        self.cancel_xshard_on_close(cid);
    }

    /// A write completed: advance progress; resubmit the remainder next loop.
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
        // L1 (2026-06-21) + A.4 (v1.25): the writev path mixes write_buf
        // bytes with arc-bulk borrowed bytes via the iovec list. A4
        // chunked writev: the SQE may cover only the leading
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
