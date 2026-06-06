//! Single-target dispatch hot path — extracted from [`crate::exec`] to
//! keep that file under the 500-LOC house rule.
//!
//! [`Shard::start_single`] is the entry point for the vast majority of
//! commands (any `Route::Local` or `Route::Single` — GET/SET/INCR/PING/
//! the lot). It first tries [`Shard::try_inline_local`], the in-order
//! reply-straight-to-conn fast path, and only falls back to the
//! pending-slot machinery when that can't fire (out-of-order seq, a
//! cross-shard hop, or a block-and-park).

use crate::Commands;
use crate::message::{Agg, Op};
use crate::shard::Shard;
use kevy_resp::{ArgvView, RespVersion};
use std::time::Instant;

impl<C: Commands> Shard<C> {
    /// Single-target command (keyless `Local` or single-key `Single`) — the
    /// overwhelming majority (GET/SET/INCR/PING/…). Skips the
    /// `Vec<(shard, Op)>` allocation + the aggregation fold loop entirely.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start_single<A: ArgvView + ?Sized>(
        &mut self,
        conn_id: u64,
        seq: u64,
        args: &A,
        shard: usize,
        is_quit: bool,
        is_write: bool,
        block_hint: crate::BlockHint,
        wake_idx: Option<u8>,
    ) {
        // Per-conn proto rides with each cmd (not the conn) so a V2 + V3
        // mix on the same owning shard each gets the right reply shape.
        // 1-byte enum copy; RESP2 client's default V2 makes every `match
        // proto` downstream a predicted no-branch.
        let proto = self.conns.get(&conn_id).map_or(RespVersion::V2, |c| c.proto);
        // In-order local fast path: `seq == next_emit` and no prior cmd is
        // pending, so write straight to the conn's output and return.
        if shard == self.id
            && self.try_inline_local(
                conn_id, args, is_quit, is_write, proto, block_hint, wake_idx,
            )
        {
            return;
        }
        self.push_pending_slot(conn_id, 1, Agg::First(None), is_quit);
        if shard == self.id {
            // Local-but-not-fast-path: only here we need an owned Argv to
            // hand to exec_op via Op::Dispatch.
            let part = self.exec_op(Op::Dispatch(args.to_argv(), proto));
            self.fold(conn_id, seq, part);
        } else {
            // Cross-shard forward: materialise owned at the handoff. The
            // -c50 single-shard hot path never reaches here.
            self.request_batch[shard].push((conn_id, seq, args.to_argv(), proto));
        }
    }

    /// Try to dispatch a single-shard local command straight to the
    /// connection's output buffer (no PendingSlot, no fold, no reply Vec).
    /// Only valid when `seq == next_emit`, i.e. nothing is pending. Returns
    /// `true` iff the inline write happened (caller skips fallback paths).
    #[inline]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn try_inline_local<A: ArgvView + ?Sized>(
        &mut self,
        conn_id: u64,
        args: &A,
        is_quit: bool,
        is_write: bool,
        proto: RespVersion,
        block_hint: crate::BlockHint,
        wake_idx: Option<u8>,
    ) -> bool {
        let Some(conn) = self.conns.get_mut(&conn_id) else { return false };
        if !conn.pending.is_empty() {
            return false;
        }
        let wrote_reply = self.dispatch_inline(conn_id, args, proto);
        // Park-on-miss for BLPOP / BRPOP / XREAD BLOCK whose dispatch_into
        // could not satisfy itself (empty list / no fresh stream entry)
        // and so wrote nothing. Skip the post-dispatch housekeeping —
        // the reply is deferred to the wake / timeout path.
        if !wrote_reply
            && let crate::BlockHint::Block { kind, key, timeout_ms } = block_hint
        {
            self.park_blocked(conn_id, args, kind, key, timeout_ms, proto);
            return true;
        }
        let Some(conn) = self.conns.get_mut(&conn_id) else { return true };
        conn.next_emit += 1;
        if is_quit {
            conn.closing = true;
        }
        if is_write {
            self.post_write_housekeeping(args, wake_idx);
        }
        true
    }

    /// Run the verb's [`Commands::dispatch_into`] arm straight into the
    /// conn's output buffer, with SLOWLOG timing wrapped around it.
    /// Returns whether the dispatch actually emitted a reply (false for a
    /// blocking command's park-on-miss arm). Caller must have already
    /// verified `conn.pending.is_empty()` and that the conn exists.
    fn dispatch_inline<A: ArgvView + ?Sized>(
        &mut self,
        conn_id: u64,
        args: &A,
        proto: RespVersion,
    ) -> bool {
        let conn = self.conns.get_mut(&conn_id).expect("caller checked");
        let out_pre_len = conn.output.len();
        // SLOWLOG OFF (`slower_than_micros < 0`) skips the clock pair so
        // the hot path stays unchanged.
        let t0 = if self.slowlog.slower_than_micros >= 0 {
            Some(Instant::now())
        } else {
            None
        };
        // Disjoint field borrows: commands / store / conn.output. Branch
        // on per-conn proto. V2 is the default + the hot path the bench
        // numbers measure; the V3 arm only fires after HELLO 3.
        match proto {
            RespVersion::V2 => self
                .commands
                .dispatch_into(&mut self.store, args, &mut conn.output),
            RespVersion::V3 => self
                .commands
                .dispatch_into_resp3(&mut self.store, args, &mut conn.output),
        }
        if let Some(t0) = t0 {
            let elapsed = t0.elapsed().as_micros().min(u64::MAX as u128) as u64;
            self.slowlog_record(args, elapsed);
        }
        self.conns
            .get(&conn_id)
            .is_some_and(|c| c.output.len() > out_pre_len)
    }

    /// Register the conn as a blocked waiter on `key` with `kind`, freeze
    /// the argv via `Commands::resolve_block_argv` (lets the command set
    /// substitute state-dependent positional args — XREAD's `$` for the
    /// stream's current last_id), and mark `Conn.blocked` so the reactor
    /// skips reading more input from this socket until wake / timeout.
    fn park_blocked<A: ArgvView + ?Sized>(
        &mut self,
        conn_id: u64,
        args: &A,
        kind: crate::BlockKind,
        key: Vec<u8>,
        timeout_ms: u64,
        proto: RespVersion,
    ) {
        let deadline_ms = if timeout_ms == 0 {
            u64::MAX
        } else {
            crate::blocked::unix_now_ms().saturating_add(timeout_ms)
        };
        let argv = self
            .commands
            .resolve_block_argv(&mut self.store, args, kind);
        let keys = [key];
        self.blocked
            .add(conn_id, &keys, kind, deadline_ms, argv, proto);
        if let Some(conn) = self.conns.get_mut(&conn_id) {
            conn.blocked = true;
        }
    }

    /// Post-`dispatch_into` work for a write that landed in the inline
    /// fast path: WATCH version bump, AOF append, keyspace notify, and
    /// BLOCK reactor wake on the written key. Each step is a no-op when
    /// its feature is unused on this shard.
    fn post_write_housekeeping<A: ArgvView + ?Sized>(
        &mut self,
        args: &A,
        wake_idx: Option<u8>,
    ) {
        // `bump_watch_for_dispatch` is an empty-map lookup when no key on
        // this shard has ever been WATCH-ed; `maybe_notify_dispatch` is
        // an empty-flags check when notify_keyspace_events is off (the
        // default), so the steady-state cost is two predicted branches.
        self.bump_watch_for_dispatch(args);
        if self.aof.is_some() {
            self.log(args);
        }
        self.maybe_notify_dispatch(args);
        // BLOCK wake: if this write targets a key that a `BLPOP` / `XREAD
        // BLOCK` waiter is parked on, wake the oldest one and retry its
        // command. Gated on `wake_idx` (None for non-wake writes) AND on
        // `BlockedClients::is_empty()`, so a None-only workload pays one
        // Option discriminant check on every write.
        if let Some(idx) = wake_idx
            && !self.blocked.is_empty()
            && let Some(key) = args.get(idx as usize).map(<[u8]>::to_vec)
        {
            self.wake_blocked_on_key(&key);
        }
    }
}
