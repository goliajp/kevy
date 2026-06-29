//! v1.29 B2-alt — bareset local-shard cancel/single-shot/re-arm
//! cycle handlers. Split out of [`crate::uring_bigbulk`] so that file
//! stays under the 500-LOC house rule; every method here is on the
//! same `impl<C: Commands> Shard<C>`.
//!
//! Flow per local-shard bare-`SET key <BIG>`:
//!
//! 1. `try_promote_bigbulk` (in `uring_bigbulk.rs`) installs
//!    `BigArgState::BareSetCancelling` + sets `big_arg_cancel_pending`.
//! 2. `uring_arm_conns` (in `uring_arm.rs`) submits the
//!    `IORING_OP_ASYNC_CANCEL` SQE targeting the multishot recv.
//! 3. Two CQEs flip the cancel flags (any order):
//!    - `OP_BIG_CANCEL` → [`Shard::uring_on_big_arg_cancel`] sets
//!      `cancel_acked`.
//!    - Terminal `OP_RECV` with `res = -ECANCELED` →
//!      [`Shard::uring_on_big_arg_target_canceled`] sets
//!      `target_canceled`.
//! 4. Both flags set → [`Shard::transition_to_reading`] flips the
//!    state to `BareSetReading` + sets `big_arg_read_pending`.
//! 5. `uring_arm_conns` submits the single-shot `prep_read` SQE
//!    pointing at `body.as_mut_ptr().add(body.len())` for the
//!    remaining bytes. Kernel writes recv bytes directly into the
//!    body Vec — no userspace memcpy.
//! 6. `OP_BIG_READ` CQE → [`Shard::uring_on_big_arg_read`] advances
//!    body via `set_len`. If incomplete, re-schedule prep_read; if
//!    complete, [`Shard::dispatch_bareset_owned`] runs the
//!    SET + sets `big_arg_rearm_recv`.
//! 7. `uring_arm_conns` re-arms the multishot for the next request.

use crate::Commands;
use crate::shard::Shard;
use crate::uring_conn::{BigArgState, UringConn};
use kevy_map::KevyMap;

impl<C: Commands> Shard<C> {
    /// **v1.29 B2-alt** — handler for `OP_BIG_READ` CQE: extend the
    /// body Vec by the kernel-reported byte count (the kernel wrote
    /// directly into `body.as_mut_ptr().add(body.len())` for `res`
    /// bytes). If body still incomplete, mark the conn for another
    /// `prep_read` on the next arm pass; if complete, dispatch + mark
    /// for multishot re-arm.
    pub(crate) fn uring_on_big_arg_read(
        &mut self,
        cid: u64,
        res: i32,
        io: &mut KevyMap<u64, UringConn>,
    ) {
        let Some(uc) = io.get_mut(&cid) else { return };
        if res <= 0 {
            // EOF or error mid-body — drop the conn (mirrors
            // `uring_on_recv` semantics; partial-body state is
            // unrecoverable here).
            uc.pending_big_arg = None;
            uc.big_arg_read_pending = false;
            uc.big_arg_rearm_recv = false;
            self.uring_mark_closing(cid, io);
            return;
        }
        let Some(state) = uc.pending_big_arg.as_mut() else { return };
        let BigArgState::BareSetReading { body, body_len, .. } = state.as_mut() else {
            // Not in reading phase — defensive ignore.
            return;
        };
        let n = res as usize;
        let cap = *body_len + 2;
        let new_len = (body.len() + n).min(cap);
        // SAFETY: the kernel wrote `n` bytes into the Vec's heap buffer
        // starting at `body.len()`. Capacity was sized to `cap` at
        // promote; the partial-read state machine never sets
        // `prep_read` larger than `cap - body.len()`, so the kernel
        // can't overrun.
        unsafe {
            body.set_len(new_len);
        }
        if body.len() == cap {
            // Body fully received — dispatch + re-arm multishot.
            if let Some(boxed) = uc.pending_big_arg.take()
                && let BigArgState::BareSetReading { key, body, body_len } = *boxed
            {
                self.dispatch_bareset_owned(cid, key, body, body_len, io);
            }
            if let Some(uc) = io.get_mut(&cid) {
                uc.big_arg_read_pending = false;
                uc.big_arg_rearm_recv = true;
            }
            self.mark_arm_pending(cid, io);
        } else {
            // More body bytes pending — schedule another prep_read.
            uc.big_arg_read_pending = true;
            self.mark_arm_pending(cid, io);
        }
    }

    /// **v1.29 B2-alt** — handler for `OP_BIG_CANCEL` CQE: mark the
    /// cancel side ack'd. If the target ECANCELED has also been seen,
    /// transition to `BareSetReading` + schedule the single-shot read.
    pub(crate) fn uring_on_big_arg_cancel(
        &mut self,
        cid: u64,
        _res: i32,
        io: &mut KevyMap<u64, UringConn>,
    ) {
        // res may be 0 (matched-cancel), -ENOENT (target already gone),
        // or -EALREADY (target executing). All three end the cancel
        // side — proceed to transition checks.
        let Some(uc) = io.get_mut(&cid) else { return };
        let Some(state) = uc.pending_big_arg.as_mut() else {
            // The body completed via multishot slabs while the cancel
            // was in flight — request a multishot re-arm so the conn
            // returns to normal mode.
            uc.big_arg_rearm_recv = true;
            self.mark_arm_pending(cid, io);
            return;
        };
        let BigArgState::BareSetCancelling {
            cancel_acked,
            target_canceled,
            ..
        } = state.as_mut()
        else {
            return;
        };
        *cancel_acked = true;
        if *cancel_acked && *target_canceled {
            self.transition_to_reading(cid, io);
        }
    }

    /// **v1.29 B2-alt** — called by `uring_on_recv` when the multishot
    /// recv's terminal CQE arrives with `res == -ECANCELED`. Mirrors
    /// [`Self::uring_on_big_arg_cancel`] on the target-side flag.
    pub(crate) fn uring_on_big_arg_target_canceled(
        &mut self,
        cid: u64,
        io: &mut KevyMap<u64, UringConn>,
    ) {
        let Some(uc) = io.get_mut(&cid) else { return };
        let Some(state) = uc.pending_big_arg.as_mut() else {
            uc.big_arg_rearm_recv = true;
            self.mark_arm_pending(cid, io);
            return;
        };
        let BigArgState::BareSetCancelling {
            cancel_acked,
            target_canceled,
            ..
        } = state.as_mut()
        else {
            return;
        };
        *target_canceled = true;
        // Multishot is gone — caller (`uring_on_recv`) already sets
        // `recv_armed = false` on !has_more; redundant here for clarity.
        uc.recv_armed = false;
        if *cancel_acked && *target_canceled {
            self.transition_to_reading(cid, io);
        }
    }

    /// **v1.29 B2-alt** — `BareSetCancelling` → `BareSetReading`
    /// transition: the multishot is fully drained; queue the
    /// single-shot `prep_read` for any remaining body bytes. If the
    /// body completed via in-flight multishot CQEs BEFORE the
    /// transition fired, dispatch immediately and request re-arm.
    pub(crate) fn transition_to_reading(
        &mut self,
        cid: u64,
        io: &mut KevyMap<u64, UringConn>,
    ) {
        let Some(uc) = io.get_mut(&cid) else { return };
        let Some(state) = uc.pending_big_arg.take() else { return };
        let BigArgState::BareSetCancelling { key, body, body_len, .. } = *state else {
            // Defensive: not the variant we expected. Drop state — the
            // conn would be wedged either way; better to lose the conn
            // than to leak the state machine.
            return;
        };
        if body.len() == body_len + 2 {
            // Body already complete (last multishot CQE finished it
            // before transition fired) — dispatch + re-arm.
            self.dispatch_bareset_owned(cid, key, body, body_len, io);
            if let Some(uc) = io.get_mut(&cid) {
                uc.big_arg_rearm_recv = true;
            }
            self.mark_arm_pending(cid, io);
            return;
        }
        uc.pending_big_arg = Some(Box::new(BigArgState::BareSetReading {
            key,
            body,
            body_len,
        }));
        uc.big_arg_read_pending = true;
        self.mark_arm_pending(cid, io);
    }

    /// **v1.29 B2-alt** — dispatch a bare `SET key <BIG>` command with
    /// an owned body Vec. Strips the trailing CRLF, runs all post-write
    /// hooks (AOF / replication / keyspace notify / BLOCK wake / WATCH
    /// bump / Lua wake bridge) on a borrowed three-slice argv view,
    /// then hands the Vec to `store.set` (consumed). Reply `+OK\r\n`
    /// goes to `conn.output`; caller marks arm-pending for the write
    /// SQE.
    pub(crate) fn dispatch_bareset_owned(
        &mut self,
        cid: u64,
        key: Vec<u8>,
        mut body: Vec<u8>,
        body_len: usize,
        io: &mut KevyMap<u64, UringConn>,
    ) {
        // Drop the trailing CRLF — `body[0..body_len]` is the value.
        body.truncate(body_len);
        let view = ThreeSliceView {
            verb: b"SET",
            key: &key,
            body: &body,
        };
        if self.aof.is_some() {
            self.log_write(&view);
        }
        if let Some(src) = self.replicate.as_mut()
            && !crate::replication_gate::is_applying_replicated()
        {
            src.push_mutation(&view);
        }
        self.maybe_notify_dispatch(&view);
        self.wake_key(&key);
        let _ok = self.store.set(&key, body, None, false, false);
        self.store.bump_if_watched(&key);
        let lua_wakes = crate::lua_wake_bridge::drain_lua_wake_buffer();
        for k in lua_wakes {
            self.wake_key(&k);
        }
        if let Some(c) = self.conns.get_mut(&cid) {
            c.output.extend_from_slice(b"+OK\r\n");
        }
        self.mark_arm_pending(cid, io);
    }
}

// =====================================================================
// v1.29 B2-alt — three-slice borrowed ArgvView for the bareset fast
// path. Implements `kevy_resp::ArgvView` so AOF / replication /
// keyspace-notification hooks accept it without materialising an owned
// `Argv` (which would memcpy the 64 KiB body).
// =====================================================================

pub(crate) struct ThreeSliceView<'a> {
    pub(crate) verb: &'a [u8],
    pub(crate) key: &'a [u8],
    pub(crate) body: &'a [u8],
}

impl<'a> core::ops::Index<usize> for ThreeSliceView<'a> {
    type Output = [u8];
    fn index(&self, i: usize) -> &[u8] {
        match i {
            0 => self.verb,
            1 => self.key,
            2 => self.body,
            _ => panic!("ThreeSliceView index oob: {i}"),
        }
    }
}

impl<'a> kevy_resp::ArgvView for ThreeSliceView<'a> {
    fn len(&self) -> usize {
        3
    }
    fn get(&self, i: usize) -> Option<&[u8]> {
        match i {
            0 => Some(self.verb),
            1 => Some(self.key),
            2 => Some(self.body),
            _ => None,
        }
    }
}
