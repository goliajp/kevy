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
use crate::message::{Agg, DispatchMeta};
use crate::shard::Shard;
use kevy_resp::{ArgvView, RespVersion};
use kevy_store::Store;
use std::time::Instant;

/// Dispatch `args` into `out` under the per-command protocol version.
/// V2 is the default + the hot path; the V3 arm only fires after a HELLO 3
/// negotiation upstream. A free function over the disjoint `Shard` fields
/// so both the inline fast path (`out` = the conn's output buffer, borrowed
/// from `self.conns`) and `run_dispatch` (`out` = the reply scratch) share
/// it.
#[inline]
pub(crate) fn dispatch_proto<C: Commands, A: ArgvView + ?Sized>(
    commands: &C,
    store: &mut Store,
    args: &A,
    proto: RespVersion,
    out: &mut Vec<u8>,
) {
    match proto {
        RespVersion::V2 => commands.dispatch_into(store, args, out),
        RespVersion::V3 => commands.dispatch_into_resp3(store, args, out),
    }
}

impl<C: Commands> Shard<C> {
    /// Single-target command (keyless `Local` or single-key `Single`) — the
    /// overwhelming majority (GET/SET/INCR/PING/…). Skips the
    /// `Vec<(shard, Op)>` allocation + the aggregation fold loop entirely.
    /// `meta` carries the origin resolve()'s write-side facts so no later
    /// stage (local post-write or the owning shard) re-parses the verb.
    /// `proto` rides per-cmd from `handle_command`'s single conns probe so
    /// a V2 + V3 mix on the same owning shard each gets the right reply
    /// shape.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start_single<A: ArgvView + ?Sized>(
        &mut self,
        conn_id: u64,
        seq: u64,
        proto: RespVersion,
        args: &A,
        shard: usize,
        is_quit: bool,
        block_hint: crate::BlockHint,
        meta: DispatchMeta,
    ) {
        // In-order local fast path: `seq == next_emit` and no prior cmd is
        // pending, so write straight to the conn's output and return.
        if shard == self.id
            && self.try_inline_local(conn_id, args, is_quit, proto, block_hint, meta)
        {
            return;
        }
        self.push_pending_slot(conn_id, 1, Agg::First(None), is_quit);
        if shard == self.id {
            // Local-but-not-fast-path (a prior cmd is still pending):
            // dispatch straight off the borrowed argv — no owned
            // materialise needed.
            let part = self.run_dispatch(args, proto, meta);
            self.fold(conn_id, seq, part);
        } else {
            // Cross-shard forward: materialise owned at the handoff —
            // into a pool-recycled Argv, so the steady state mallocs
            // nothing. The -c50 single-shard hot path never reaches here.
            let argv = self.argv_pool.take_filled(args);
            self.request_batch[shard].push((conn_id, seq, argv, proto, meta));
            self.request_batch_nonempty |= 1u64 << shard;
        }
    }

    /// Try to dispatch a single-shard local command straight to the
    /// connection's output buffer (no PendingSlot, no fold, no reply Vec).
    /// Only valid when `seq == next_emit`, i.e. nothing is pending. Returns
    /// `true` iff the inline write happened (caller skips fallback paths).
    ///
    /// One conns probe covers the whole inline path — pending check,
    /// dispatch into `conn.output` (disjoint field borrows: commands /
    /// store / conn), wrote-reply check, and the next_emit/closing
    /// bookkeeping. This used to be four probes split across a
    /// `dispatch_inline` helper.
    #[inline]
    pub(crate) fn try_inline_local<A: ArgvView + ?Sized>(
        &mut self,
        conn_id: u64,
        args: &A,
        is_quit: bool,
        proto: RespVersion,
        block_hint: crate::BlockHint,
        meta: DispatchMeta,
    ) -> bool {
        // Field-only read, before the conn borrow.
        let t0 = self.slowlog_t0();
        let Some(conn) = self.conns.get_mut(&conn_id) else { return false };
        if !conn.pending.is_empty() {
            return false;
        }
        let out_pre_len = conn.output.len();
        dispatch_proto(&self.commands, &mut self.store, args, proto, &mut conn.output);
        let wrote_reply = conn.output.len() > out_pre_len;
        // Park-on-miss for BLPOP / BRPOP / XREAD BLOCK that wrote nothing:
        // the reply is deferred to the wake / timeout path.
        if !wrote_reply
            && let crate::BlockHint::Block { kind, keys, timeout_ms } = block_hint
        {
            self.slowlog_maybe(t0, args);
            self.park_dispatch(conn_id, args, kind, keys, timeout_ms, proto);
            return true;
        }
        conn.next_emit += 1;
        if is_quit {
            conn.closing = true;
        }
        self.slowlog_maybe(t0, args);
        if meta.is_write {
            self.post_write_housekeeping(args, meta);
        }
        true
    }

    /// SLOWLOG start instant — `None` when SLOWLOG is OFF
    /// (`slower_than_micros < 0`, the default), skipping the clock pair.
    #[inline]
    pub(crate) fn slowlog_t0(&self) -> Option<Instant> {
        if self.slowlog.slower_than_micros >= 0 {
            Some(Instant::now())
        } else {
            None
        }
    }

    /// Record `args` in the SLOWLOG if a start instant was captured
    /// (`None` = SLOWLOG OFF, the default — a no-op).
    pub(crate) fn slowlog_maybe<A: ArgvView + ?Sized>(&mut self, t0: Option<Instant>, args: &A) {
        if let Some(t0) = t0 {
            let elapsed = t0.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
            self.slowlog_record(args, elapsed);
        }
    }

    /// Park a blocking command whose `dispatch_into` produced no reply.
    /// Picks the strategy from the watched keys:
    /// - **single key on this shard** → the in-shard fast path
    ///   ([`crate::blocked::BlockedClients`]): freeze the replay argv (via
    ///   `block_serve_argv` then `resolve_block_argv` for `$`) and register
    ///   the waiter locally — no cross-core hop.
    /// - **single remote key, or any multi-key form** → the cross-shard
    ///   arbiter ([`crate::block_xshard`]): the conn parks here (its origin
    ///   shard) and watch registrations fan out to each key's owning shard.
    ///
    /// Either way `Conn.blocked` is set so the reactor knows the conn is
    /// parked until a wake or timeout.
    fn park_dispatch<A: ArgvView + ?Sized>(
        &mut self,
        conn_id: u64,
        args: &A,
        kind: crate::BlockKind,
        keys: Vec<Vec<u8>>,
        timeout_ms: u64,
        proto: RespVersion,
    ) {
        let deadline_ms = if timeout_ms == 0 {
            u64::MAX
        } else {
            crate::blocked::unix_now_ms().saturating_add(timeout_ms)
        };
        if keys.len() == 1 && self.shard_of(&keys[0]) == self.id {
            // In-shard fast path: narrow to the one key + freeze `$`.
            let serve = self.commands.block_serve_argv(args, kind, &keys[0]);
            let serve = self.commands.resolve_block_argv(&mut self.store, &serve, kind);
            self.blocked.add(
                conn_id,
                std::slice::from_ref(&keys[0]),
                kind,
                deadline_ms,
                serve,
                proto,
            );
            if let Some(conn) = self.conns.get_mut(&conn_id) {
                conn.blocked = true;
            }
        } else {
            let entries =
                crate::block_xshard::build_serve_entries(&self.commands, args, kind, &keys);
            self.park_blocked_xshard(conn_id, kind, entries, deadline_ms, proto);
        }
    }

    /// Post-`dispatch_into` work for a write — runs after the inline fast
    /// path here and after [`Shard::run_dispatch`] (the local fallback +
    /// forwarded paths): WATCH version bump, AOF append, keyspace notify,
    /// and BLOCK reactor wake on the written key. Each step is a no-op
    /// when its feature is unused on this shard.
    pub(crate) fn post_write_housekeeping<A: ArgvView + ?Sized>(
        &mut self,
        args: &A,
        meta: DispatchMeta,
    ) {
        // The WATCH bump uses `meta.key_idx` straight from the origin
        // resolve() — no verb re-parse (this used to re-run the ~40-arm
        // `Commands::route` walk on every local write). `bump_if_watched`
        // is an empty-map lookup when nothing is WATCH-ed;
        // `maybe_notify_dispatch` is an empty-flags check when
        // notify_keyspace_events is off (the default).
        if let Some(idx) = meta.key_idx
            && (idx as usize) < args.len()
        {
            self.store.bump_if_watched(&args[idx as usize]);
        }
        if self.aof.is_some() {
            self.log_write(args);
        }
        // Replication: when `[replication] role = "primary"`, push the
        // applied mutation to this shard's backlog so connected replicas
        // can stream it. Generic over ArgvView so no `Argv` is
        // materialised on the borrowed fast path. `None` (the default)
        // short-circuits to one Option-discriminant check.
        //
        // The `is_applying_replicated` check suppresses the push when
        // this dispatch is itself applying a frame pulled from an
        // upstream primary (T1.29 server-as-replica path). Defends
        // against chain replication / infinite re-emit in the brief
        // window during `REPLICAOF NO ONE` promotion when both an
        // upstream link and a downstream source can coexist. The
        // thread-local read is a cheap branch on the cold path here.
        if let Some(src) = self.replicate.as_mut()
            && !crate::replication_gate::is_applying_replicated()
        {
            src.push_mutation(args);
        }
        self.maybe_notify_dispatch(args);
        // BLOCK wake: if this write targets a key a waiter is parked on,
        // wake it. Gated on `wake_idx` (None for non-wake writes), so a
        // None-only workload pays one Option discriminant check per write.
        if let Some(idx) = meta.wake_idx
            && let Some(key) = args.get(idx as usize).map(<[u8]>::to_vec)
        {
            self.wake_key(&key);
        }
    }

    /// Wake both block registries for a write that landed on `key`: the
    /// in-shard fast path ([`crate::blocked::BlockedClients`]) and the
    /// cross-shard arbiter ([`crate::block_xshard`]). Each is an
    /// `is_empty()` short-circuit when unused, so the steady state pays two
    /// predicted branches. Called from the local write path here and from
    /// the cross-shard forwarded write path in [`crate::exec_op`].
    pub(crate) fn wake_key(&mut self, key: &[u8]) {
        if !self.blocked.is_empty() {
            self.wake_blocked_on_key(key);
        }
        if !self.xwaiters.is_empty() && self.xwaiters.is_watched(key) {
            self.target_wake_xshard(key);
        }
    }
}
