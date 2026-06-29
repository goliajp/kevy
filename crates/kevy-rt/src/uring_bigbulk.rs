//! **v1.25 B.4 + A.2 / B.5** — BigBulk frame-stitch ingest path for the
//! io_uring reactor.
//!
//! The Phase A decompositions
//! ([`.claude/notes/v125-deco-axis-i-c50-10kb.md`] +
//! [`.claude/notes/v125-deco-axis-b-64kb.md`]) identified the conn.input
//! realloc storm on multi-CQE big values as a key amplifier on Axis B
//! (64 KiB SET) and Axis I (10 KiB SET): the multishot recv path splits
//! the body into ~16 KiB chunks; each chunk gets memcpy'd from the
//! kernel slab into `conn.input` with `Vec::extend_from_slice`, which on
//! a cold conn drives 0→16→32→48→64K reallocs as capacity grows.
//!
//! This module installs a per-conn state machine that, on detecting a
//! `*<argc> <supported-verb> … $<N>\r\n` header whose **last bulk** has
//! `N ≥ BIG_ARG_PROMOTE_THRESHOLD` AND whose body isn't fully present
//! in the slab head, pre-allocates a `Vec<u8>` sized exactly to the
//! whole RESP frame length and routes subsequent multishot recv CQE
//! bytes into THAT Vec instead of `conn.input`. On completion the
//! assembled frame is re-dispatched through `Shard::dispatch_batch`,
//! which runs the existing command handlers (SET / SETEX / PSETEX /
//! APPEND / GETSET / MSET) unchanged — same routing, same AOF, same
//! reply emission.
//!
//! **2026-06-22 (v1.25 B.5)**: the originally-shipped B.4 bare-SET
//! fast path that adopted the body Vec into the value `Arc<[u8]>`
//! zero-copy was RETIRED. That path bypassed cross-shard routing
//! (`self.store.set` writes directly to the connection's owning shard
//! rather than the key's owning shard) — a silent data-loss bug on
//! multi-shard setups when the key hashed off-shard. Test sweep:
//! single-shard cluster with multi-CQE values (≥ 16 KiB) confirmed
//! `STRLEN` returned 0 after `SET`. The frame-stitch path goes through
//! `dispatch_batch` → `handle_command` → `start_command` which honours
//! the cluster routing layer, preserving correctness. The Arc adoption
//! micro-win it gave up (~0.5–1 µs per 64 KiB SET) is a v1.25.x lever
//! to revisit once the underlying routing is plumbed for owned-Vec
//! cross-shard hand-off.
//!
//! Variants supported (last bulk must be big):
//! - `SET key <BIG>` (plain 3-arg)
//! - `SETEX key ttl <BIG>` / `PSETEX key ms <BIG>`
//! - `APPEND key <BIG>` / `GETSET key <BIG>`
//! - `MSET k1 v1 … kn <BIG>` (last value big)
//!
//! Out of scope (v1.25.x follow-up): `SET k <BIG> EX 10` (big value at
//! position #3 of 5, not last); `MSET k1 <BIG> k2 v2` (big value not
//! last). These keep the borrowed-slice path — correct but no realloc
//! savings.

use crate::Commands;
use crate::shard::Shard;
use crate::uring_bigbulk_probe::{
    BigArgGenericProbe, MAX_BULK_LEN, probe_generic_bigbulk,
};
use crate::uring_conn::{BigArgState, UringConn};
use kevy_map::KevyMap;

impl<C: Commands> Shard<C> {
    /// **v1.25 B.4 + A.2 / B.5** — try to promote the conn into BigBulk-recv
    /// mode based on `tail`'s contents. Returns `true` iff `tail`'s head
    /// matched the generic last-bulk-big shape (`*<argc> <supported-verb>
    /// … $N`) with `N ≥ BIG_ARG_PROMOTE_THRESHOLD`. On match, the full
    /// `tail.len()` bytes are copied into the assembled-frame Vec
    /// (capacity pre-sized to the entire expected frame length).
    /// Subsequent multishot CQEs feed directly into the same Vec via
    /// [`Self::uring_bigbulk_feed`].
    pub(crate) fn try_promote_bigbulk(
        &mut self,
        cid: u64,
        tail: &[u8],
        io: &mut KevyMap<u64, UringConn>,
    ) -> bool {
        let BigArgGenericProbe::Promote {
            total,
            bytes_present,
            body_start_in_tail,
            body_len,
            bare_set_key_range,
        } = probe_generic_bigbulk(tail)
        else {
            return false;
        };
        let Some(uc) = io.get_mut(&cid) else { return false };
        if uc.pending_big_arg.is_some() {
            return false;
        }
        if total > MAX_BULK_LEN + 1024 {
            // Defensive: the per-bulk MAX_BULK_LEN gate in the probe
            // already caps body size; add a small slack for headers.
            return false;
        }
        // v1.29 B2-alt — bare-SET local-shard fast path: kernel writes
        // the value body directly into an owned Vec via single-shot
        // `prep_read` (no userspace memcpy through the slab). Gated on
        // shard-affinity at promote time so the v1.25 B.4 cross-shard
        // data-loss bug never re-emerges.
        if let Some((k_start, k_end)) = bare_set_key_range {
            let key = tail[k_start..k_end].to_vec();
            if self.shard_of(&key) == self.id {
                let body_cap = body_len + 2;
                let mut body = Vec::with_capacity(body_cap);
                let body_in_slab =
                    bytes_present.saturating_sub(body_start_in_tail).min(body_cap);
                body.extend_from_slice(
                    &tail[body_start_in_tail..body_start_in_tail + body_in_slab],
                );
                if body.len() == body_cap {
                    // Whole frame in slab — dispatch immediately, no
                    // cancel/single-shot dance needed.
                    self.dispatch_bareset_owned(cid, key, body, body_len, io);
                    return true;
                }
                let Some(uc) = io.get_mut(&cid) else { return false };
                uc.pending_big_arg = Some(Box::new(BigArgState::BareSetCancelling {
                    key,
                    body,
                    body_len,
                    cancel_acked: false,
                    target_canceled: false,
                }));
                // Defer the cancel-SQE submission to the next arm pass;
                // `uring_arm_conns` checks this flag and queues
                // `prep_cancel(OP_RECV|cid, OP_BIG_CANCEL|cid)`. The
                // pending state stays correct even if a multishot CQE
                // arrives in the gap (handler routes those slab bytes
                // into `body` via `BareSetCancelling`'s feed arm).
                uc.big_arg_cancel_pending = true;
                self.mark_arm_pending(cid, io);
                return true;
            }
            // Cross-shard bare-SET: fall through to Frame path (no
            // regression vs v1.28).
        }
        // Frame path — every non-bare-SET promote + cross-shard bare-SET.
        self.install_frame_state(cid, total, bytes_present, tail, io)
    }

    /// Frame-variant install + maybe-finalize. Extracted from
    /// `try_promote_bigbulk` so the B2-alt cross-shard fallback shares it.
    fn install_frame_state(
        &mut self,
        cid: u64,
        total: usize,
        bytes_present: usize,
        tail: &[u8],
        io: &mut KevyMap<u64, UringConn>,
    ) -> bool {
        let take = bytes_present.min(total);
        let mut frame = Vec::with_capacity(total);
        frame.extend_from_slice(&tail[..take]);
        if frame.len() == total {
            self.uring_apply_frame_stitch(cid, frame, io);
            return true;
        }
        let Some(uc) = io.get_mut(&cid) else { return false };
        uc.pending_big_arg = Some(Box::new(BigArgState::Frame { frame, total }));
        true
    }

    /// **v1.25 B.5 / v1.29 B2-alt** — append multishot-recv slab bytes
    /// into the conn's in-progress big-arg dest Vec (`frame` for the
    /// Frame variant, `body` for the BareSetCancelling variant). After
    /// the cancel takes effect — both flags set in `BareSetCancelling`
    /// — incoming bytes come via single-shot `prep_read` CQEs through
    /// [`Self::uring_on_big_arg_read`] instead. BareSetReading never
    /// receives multishot CQEs (multishot is cancelled by then).
    pub(crate) fn uring_bigbulk_feed(
        &mut self,
        cid: u64,
        io: &mut KevyMap<u64, UringConn>,
        slab: &[u8],
    ) {
        let Some(uc) = io.get_mut(&cid) else { return };
        let Some(state) = uc.pending_big_arg.as_mut() else { return };
        let take = match state.as_mut() {
            BigArgState::Frame { frame, total } => {
                let need = *total - frame.len();
                let t = slab.len().min(need);
                if t > 0 {
                    frame.extend_from_slice(&slab[..t]);
                }
                if frame.len() == *total {
                    if let Some(boxed) = uc.pending_big_arg.take()
                        && let BigArgState::Frame { frame, .. } = *boxed
                    {
                        self.uring_apply_frame_stitch(cid, frame, io);
                    }
                }
                t
            }
            BigArgState::BareSetCancelling { body, body_len, .. } => {
                let cap = *body_len + 2;
                let need = cap - body.len();
                let t = slab.len().min(need);
                if t > 0 {
                    body.extend_from_slice(&slab[..t]);
                }
                if body.len() == cap {
                    // Body finished entirely via multishot slabs before
                    // the cancel pair completed. Drop the state +
                    // dispatch. **Critical**: clear `big_arg_cancel_pending`
                    // so the next arm pass doesn't submit a cancel SQE
                    // that would cancel the newly re-armed multishot
                    // (which would wedge the conn). The target ECANCELED
                    // CQE will not fire because we never submitted the
                    // cancel — the multishot stays armed.
                    uc.big_arg_cancel_pending = false;
                    if let Some(boxed) = uc.pending_big_arg.take()
                        && let BigArgState::BareSetCancelling { key, body, body_len, .. } = *boxed
                    {
                        self.dispatch_bareset_owned(cid, key, body, body_len, io);
                    }
                }
                t
            }
            BigArgState::BareSetReading { .. } => {
                // Defensive: multishot is cancelled in this phase, so a
                // CQE here shouldn't fire. If it does (kernel race), drop
                // the bytes — the conn would be wedged either way.
                0
            }
        };
        if take < slab.len() {
            self.uring_bigbulk_feed_pipelined(cid, io, &slab[take..]);
        }
    }

    // v1.29 B2-alt cancel/single-shot/re-arm handlers + bareset
    // dispatch live in `crate::uring_bigbulk_b2alt` so this file stays
    // under the 500-LOC house rule. Same `impl<C: Commands> Shard<C>`.

    /// Route pipelined bytes past the BigBulk frame through the regular
    /// dispatch path — they might be a fresh big-value command that
    /// itself promotes (the recursion bottoms out naturally), or a
    /// small command, or a partial frame that gets staged into
    /// `conn.input`.
    fn uring_bigbulk_feed_pipelined(
        &mut self,
        cid: u64,
        io: &mut KevyMap<u64, UringConn>,
        extra: &[u8],
    ) {
        let mut input_buf = match self.conns.get_mut(&cid) {
            Some(c) => std::mem::take(&mut c.input),
            None => return,
        };
        let outcome = self.uring_recv_dispatch(cid, extra, &mut input_buf, io);
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

    /// **v1.25 B.5** — finalise a FrameStitch: re-dispatch the
    /// assembled RESP frame through the normal parser. Re-uses every
    /// command handler unchanged (SET / SETEX / PSETEX / APPEND /
    /// GETSET / MSET), including the cross-shard routing layer that
    /// `handle_command` invokes via `start_command`. On protocol error
    /// mid-frame (defensive — the probe already validated headers),
    /// marks the conn closing.
    fn uring_apply_frame_stitch(
        &mut self,
        cid: u64,
        frame: Vec<u8>,
        io: &mut KevyMap<u64, UringConn>,
    ) {
        let outcome = self.dispatch_batch(cid, &frame);
        if outcome.conn_gone {
            return;
        }
        if outcome.protocol_error {
            self.protocol_error(cid);
            self.uring_mark_closing(cid, io);
            return;
        }
        // `consumed < frame.len()` should not occur — probe sized the
        // Vec exactly to one frame. If it does (parser disagrees with
        // probe), drop the residue defensively.
        debug_assert_eq!(
            outcome.consumed,
            frame.len(),
            "frame-stitch: parser consumed != probe total"
        );
    }
}

#[cfg(test)]
mod tests {
    // Probe tests live in `crate::uring_bigbulk_probe::tests`. End-to-end
    // state-machine + apply tests live alongside the rest of the reactor
    // integration tests (`crates/kevy/tests/`) — we keep this module
    // focused on the wiring helpers.
}
