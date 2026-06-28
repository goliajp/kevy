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
            body_start_in_tail: _,
            body_len: _,
            bare_set_key_range: _,
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
        let take = bytes_present.min(total);
        let mut frame = Vec::with_capacity(total);
        frame.extend_from_slice(&tail[..take]);
        if frame.len() == total {
            // Pathological: the entire frame including the big-bulk
            // body+CRLF landed in this slab. Re-dispatch immediately,
            // don't install state.
            self.uring_apply_frame_stitch(cid, frame, io);
            return true;
        }
        uc.pending_big_arg = Some(Box::new(BigArgState { frame, total }));
        true
    }

    /// **v1.25 B.4 + A.2 / B.5** — append slab bytes into the conn's
    /// `pending_big_arg.frame`, completing the frame when `frame.len() ==
    /// total`. Excess bytes past the frame end are a pipelined next
    /// command; they get routed through the regular dispatch path so the
    /// next frame in the pipeline can also promote.
    pub(crate) fn uring_bigbulk_feed(
        &mut self,
        cid: u64,
        io: &mut KevyMap<u64, UringConn>,
        slab: &[u8],
    ) {
        let Some(uc) = io.get_mut(&cid) else { return };
        let Some(state) = uc.pending_big_arg.as_mut() else { return };
        let need = state.total - state.frame.len();
        let take = slab.len().min(need);
        if take > 0 {
            state.frame.extend_from_slice(&slab[..take]);
        }
        let total_v = state.total;
        if state.frame.len() == total_v {
            let state = uc.pending_big_arg.take().expect("just observed");
            self.uring_apply_frame_stitch(cid, state.frame, io);
        }
        if take < slab.len() {
            self.uring_bigbulk_feed_pipelined(cid, io, &slab[take..]);
        }
    }

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
