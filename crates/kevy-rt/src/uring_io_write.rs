//! Write-side io_uring completion handlers: the per-tick output-limit
//! sweep and the write-progress pump. Split out of [`crate::uring_io`]
//! so that file stays under the 500-LOC house rule; every method here is
//! on the same `impl<C: Commands> Shard<C>` and is only ever called from
//! `run_uring`.

use crate::Commands;
use crate::shard::Shard;
use crate::uring_conn::UringConn;
use kevy_map::KevyMap;

impl<C: Commands> Shard<C> {
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
            self.uring_resolve_serve(cid, io);
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
                //
                // The flatten starts at `write_off`, not at zero — see
                // `uring_write_linearize` for why starting at zero
                // re-transmits an already-sent prefix and desynchronises
                // the peer's RESP framing.
                let (linear, new_off) = crate::uring_write_linearize::linearize_unsent(
                    &uc.write_buf,
                    &uc.write_arcs,
                    uc.write_off,
                    written,
                );
                uc.write_buf = linear;
                uc.write_off = new_off;
                uc.write_arcs.clear();
                uc.write_iovecs.clear();
                uc.arcs_in_flight = 0;
                uc.write_byte_cap = 0;
                uc.write_inflight_bytes = 0;
            }
            self.uring_resolve_serve(cid, io);
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
        self.uring_resolve_serve(cid, io);
    }
}
