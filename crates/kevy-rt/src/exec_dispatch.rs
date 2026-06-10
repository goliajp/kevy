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
use std::time::Instant;

impl<C: Commands> Shard<C> {
    /// Single-target command (keyless `Local` or single-key `Single`) — the
    /// overwhelming majority (GET/SET/INCR/PING/…). Skips the
    /// `Vec<(shard, Op)>` allocation + the aggregation fold loop entirely.
    /// `meta` carries the origin resolve()'s write-side facts so no later
    /// stage (local post-write or the owning shard) re-parses the verb.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start_single<A: ArgvView + ?Sized>(
        &mut self,
        conn_id: u64,
        seq: u64,
        args: &A,
        shard: usize,
        is_quit: bool,
        block_hint: crate::BlockHint,
        meta: DispatchMeta,
    ) {
        // Per-conn proto rides with each cmd (not the conn) so a V2 + V3
        // mix on the same owning shard each gets the right reply shape.
        // 1-byte enum copy; RESP2 client's default V2 makes every `match
        // proto` downstream a predicted no-branch.
        let proto = self.conns.get(&conn_id).map_or(RespVersion::V2, |c| c.proto);
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
        }
    }

    /// Try to dispatch a single-shard local command straight to the
    /// connection's output buffer (no PendingSlot, no fold, no reply Vec).
    /// Only valid when `seq == next_emit`, i.e. nothing is pending. Returns
    /// `true` iff the inline write happened (caller skips fallback paths).
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
            && let crate::BlockHint::Block { kind, keys, timeout_ms } = block_hint
        {
            self.park_dispatch(conn_id, args, kind, keys, timeout_ms, proto);
            return true;
        }
        let Some(conn) = self.conns.get_mut(&conn_id) else { return true };
        conn.next_emit += 1;
        if is_quit {
            conn.closing = true;
        }
        if meta.is_write {
            self.post_write_housekeeping(args, meta);
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
        if keys.len() == 1 && crate::reduce::shard_of(&keys[0], self.nshards) == self.id {
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
