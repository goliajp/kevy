//! Command execution: the half of [`Shard`] that turns parsed commands into
//! shard-local work and reduces the (possibly multi-shard) results.
//!
//! [`crate::shard`] owns the reactor (sockets, the inbound queue, flushing);
//! this module owns the *semantics* — transaction state, routing a command to
//! the shard(s) that own its keys, executing one op against the local store,
//! and folding sub-results into each connection's seq-ordered ring.

use crate::exec_fold::relative_ttl_write;
use crate::message::{Agg, DispatchMeta, Inbound, Op, Part, PendingSlot, SmallReply};
use crate::shard::Shard;
use crate::{Commands, ResolvedCmd, Route, TxnKind};
use kevy_resp::{Argv, ArgvView, RespVersion, encode_array_len};

impl<C: Commands> Shard<C> {
    /// Apply transaction state (queue inside MULTI), else dispatch the command.
    pub(crate) fn handle_command<A: ArgvView + ?Sized>(&mut self, conn_id: u64, args: &A) {
        // CLIENT SETNAME / CLIENT GETNAME intercept.
        // These need per-conn state which the
        // stateless `cmd_client` dispatch can't access; handle in-line
        // here where we already own `&mut Conn` via `self.conns`. All
        // other CLIENT subcommands fall through to the standard
        // dispatch unchanged.
        if Self::try_intercept_client(self, conn_id, args) {
            return;
        }
        // One verb-resolution per cmd (was 4: txn_kind + route + is_quit +
        // is_write each scanned the verb separately). KevyCommands overrides
        // resolve() with a single match; non-overriding impls still pay 4×.
        let resolved = self.commands.resolve(args);
        // One conns probe serves the whole pre-dispatch phase — the MULTI
        // check, the per-cmd proto capture, and (for the dispatching hot
        // arms) the seq assignment. These were three separate map probes
        // per command (in_multi here + next_seq_for + start_single's proto
        // read).
        let Some(c) = self.conns.get_mut(&conn_id) else { return };
        let in_multi = c.multi.is_some();
        let proto = c.proto;
        let cluster_conn = c.cluster;
        if !in_multi && matches!(resolved.txn_kind, TxnKind::Other | TxnKind::Watch) {
            let seq = c.next_seq;
            c.next_seq += 1;
            self.start_command(conn_id, seq, proto, args, resolved, cluster_conn);
            return;
        }
        self.handle_txn_state(conn_id, in_multi, &resolved.txn_kind, args);
    }

    /// Transaction-state arms of [`Self::handle_command`] (rare next to
    /// the dispatch path there); each re-probes internally / via
    /// immediate_reply. Extracted verbatim (single call site,
    /// `inline(always)`) purely for the 50-LOC fn rule — codegen is the
    /// manual-inline equivalent.
    #[inline(always)]
    fn handle_txn_state<A: ArgvView + ?Sized>(
        &mut self,
        conn_id: u64,
        in_multi: bool,
        txn_kind: &TxnKind,
        args: &A,
    ) {
        match (in_multi, txn_kind) {
            (false, TxnKind::Multi) => {
                if let Some(c) = self.conns.get_mut(&conn_id) {
                    c.multi = Some(Vec::new());
                }
                self.immediate_reply(conn_id, b"+OK\r\n".to_vec());
            }
            (false, TxnKind::Exec) => {
                self.immediate_reply(conn_id, b"-ERR EXEC without MULTI\r\n".to_vec());
            }
            (false, TxnKind::Discard) => {
                self.immediate_reply(conn_id, b"-ERR DISCARD without MULTI\r\n".to_vec());
            }
            (true, TxnKind::Multi) => {
                self.immediate_reply(conn_id, b"-ERR MULTI calls can not be nested\r\n".to_vec());
            }
            (true, TxnKind::Discard) => {
                // DISCARD drops the queued cmds AND any `WATCH`-ed keys
                // (Redis semantics — see https://redis.io/commands/discard).
                if let Some(c) = self.conns.get_mut(&conn_id) {
                    c.multi = None;
                    c.watched.clear();
                }
                self.immediate_reply(conn_id, b"+OK\r\n".to_vec());
            }
            (true, TxnKind::Exec) => self.exec_transaction(conn_id),
            (true, TxnKind::Watch) => self.immediate_reply(
                conn_id,
                b"-ERR WATCH inside MULTI is not allowed\r\n".to_vec(),
            ),
            (true, TxnKind::Other) => {
                if let Some(q) = self.conns.get_mut(&conn_id).and_then(|c| c.multi.as_mut()) {
                    q.push(args.to_argv());
                }
                self.immediate_reply(conn_id, b"+QUEUED\r\n".to_vec());
            }
            // (false, Other | Watch) dispatched on the early path above.
            (false, TxnKind::Other | TxnKind::Watch) => {}
        }
    }

    /// Push a slot that resolves immediately to `bytes` (preserves seq order).
    pub(crate) fn immediate_reply(&mut self, conn_id: u64, bytes: Vec<u8>) {
        let seq = match self.conns.get_mut(&conn_id) {
            Some(c) => {
                let s = c.next_seq;
                c.next_seq += 1;
                s
            }
            None => return,
        };
        if let Some(c) = self.conns.get_mut(&conn_id) {
            let proto = c.proto;
            c.pending.push_back(PendingSlot {
                remaining: 1,
                agg: Agg::First(None),
                done: None,
                proto,
            });
        }
        self.fold(conn_id, seq, Part::Reply(SmallReply::from_vec(bytes)));
    }

    /// `EXEC` — emit a `*N` array header, then run the queued commands in order.
    /// The seq-ordered ring concatenates their replies into one valid array.
    /// If the conn has any `WATCH`-ed keys, delegate to the pre-check fan-out
    /// path in [`crate::exec_watch`] (aborts if any watched key is dirty).
    fn exec_transaction(&mut self, conn_id: u64) {
        let (queued, watched) = match self.conns.get_mut(&conn_id) {
            Some(c) => (
                c.multi.take().unwrap_or_default(),
                std::mem::take(&mut c.watched),
            ),
            None => return,
        };
        if !watched.is_empty() {
            self.exec_transaction_watched(conn_id, queued, watched);
            return;
        }
        let mut header = Vec::new();
        encode_array_len(&mut header, queued.len() as i64);
        self.immediate_reply(conn_id, header);
        for cmd in &queued {
            let resolved = self.commands.resolve(cmd);
            // EXEC's queued cmds inherit the conn's proto at execution
            // time (same per-cmd capture as the live dispatch path).
            let Some(c) = self.conns.get_mut(&conn_id) else { return };
            let seq = c.next_seq;
            c.next_seq += 1;
            let proto = c.proto;
            // cluster_conn = false: queued transactions execute with full
            // cross-shard fan-out even on a cluster conn (superset
            // behaviour — the redirect already happened, or never will).
            self.start_command(conn_id, seq, proto, cmd, resolved, false);
        }
    }

    /// Hand off to the per-shape starter (pub/sub / single-target /
    /// multi-target). Each starter owns the rest of the command's life
    /// cycle: pending-slot bookkeeping, local exec, and cross-shard
    /// forwarding. `seq` and `proto` arrive from the caller's single
    /// pre-dispatch conns probe (see [`Self::handle_command`]).
    // LOC-WAIVER: data-driven route dispatch table — one arm per Route
    // variant, each a one-line handoff to that route's starter.
    fn start_command<A: ArgvView + ?Sized>(
        &mut self,
        conn_id: u64,
        seq: u64,
        proto: RespVersion,
        args: &A,
        resolved: ResolvedCmd,
        cluster_conn: bool,
    ) {
        // One client command at the dispatch boundary (before fan-out, so a
        // multi-key command counts once) — INFO's total_commands_processed.
        self.commands.on_command();
        let ResolvedCmd {
            route,
            is_quit,
            is_write,
            block_hint,
            wake_idx,
            ..
        } = resolved;
        // Role-gated write rejection (read-only replica).
        // `seq` is already assigned by handle_command — resolve it
        // directly (immediate_reply would double-assign and wedge the
        // emit order).
        if is_write && let Some(err) = self.commands.write_denied() {
            self.push_pending_slot(conn_id, 1, Agg::First(None), false);
            self.fold(conn_id, seq, Part::Reply(SmallReply::from_vec(err)));
            return;
        }
        if !is_write && let Some(err) = self.commands.read_denied(args) {
            self.push_pending_slot(conn_id, 1, Agg::First(None), false);
            self.fold(conn_id, seq, Part::Reply(SmallReply::from_vec(err)));
            return;
        }
        match route {
            Route::Subscribe => self.do_subscribe(conn_id, seq, args, true),
            Route::Unsubscribe => self.do_subscribe(conn_id, seq, args, false),
            Route::Psubscribe => self.do_psubscribe(conn_id, seq, args),
            Route::Punsubscribe => self.do_punsubscribe(conn_id, seq, args),
            Route::Publish => self.do_publish(conn_id, seq, args),
            Route::Watch => self.do_watch(conn_id, seq, args),
            Route::Unwatch => self.do_unwatch(conn_id, seq),
            Route::Hello => self.do_hello(conn_id, seq, args),
            Route::Rename { nx } => self.start_rename(conn_id, seq, args, nx),
            // FEED.* — parse + shard-index dispatch live in
            // [`crate::exec_feed`] (500-LOC house rule).
            r @ (Route::FeedShards | Route::FeedTail | Route::FeedRead) => {
                self.start_feed_route(conn_id, seq, args, &r, is_quit);
            }
            Route::Slowlog(sub) => self.start_slowlog(conn_id, seq, sub),
            // WAIT / REPL.WAIT — deferred all-shard barriers; own
            // starters (not dispatch_targets) so a parked waiter never
            // rides `xshard_inflight` (see [`crate::exec_replwait`]).
            Route::ReplWait { numreplicas, timeout_ms } => {
                self.start_repl_wait(conn_id, seq, numreplicas, timeout_ms);
            }
            Route::ReplBarrier { offsets, timeout_ms, miss } => {
                self.start_repl_barrier(conn_id, seq, offsets, timeout_ms, miss);
            }
            Route::Local => {
                let meta = DispatchMeta { is_write, wake_idx, key_idx: None };
                self.start_single(conn_id, seq, proto, args, self.id, is_quit, block_hint, meta);
            }
            Route::Single(idx) => {
                let shard = self.shard_of(&args[idx]);
                // Cluster conns own their shard's slots only: a wrong-shard
                // key redirects (`-MOVED`) instead of forwarding, keeping a
                // cluster client's topology honest. `cluster_conn` is only
                // ever true in cluster mode, so the compat / cluster-off
                // path pays one always-false branch. (EXEC replay passes
                // false — queued transactions keep full fan-out semantics.)
                if cluster_conn
                    && shard != self.id
                    && let Some(topo) = &self.cluster
                {
                    let slot = kevy_hash::key_hash_slot(&args[idx]);
                    let bytes = topo.moved(slot, shard);
                    self.push_pending_slot(conn_id, 1, Agg::First(None), is_quit);
                    self.fold(conn_id, seq, Part::Reply(SmallReply::from_vec(bytes)));
                    return;
                }
                // Keyed routes put the key at argv[1] (or argv[2] for
                // XGROUP/XINFO) — well inside u8.
                let meta = DispatchMeta { is_write, wake_idx, key_idx: Some(idx as u8) };
                self.start_single(conn_id, seq, proto, args, shard, is_quit, block_hint, meta);
            }
            // Cluster conns get `-CROSSSLOT` on cross-slot multi-key
            // (MGET/MSET/SINTER/SUNION/SDIFF); else fan-out as before.
            other => self.start_multi_or_crossslot(
                conn_id, seq, args, other, is_quit, cluster_conn,
            ),
        }
    }

    // `start_single` + `try_inline_local` (and their helpers `park_blocked`
    // / `post_write_housekeeping`) live in [`crate::exec_dispatch`] —
    // same `impl<C: Commands> Shard<C>`, split out so this file stays
    // under the 500-LOC house rule. CROSSSLOT helpers live in
    // [`crate::exec_crossslot`] for the same reason.

    /// Multi-target / aggregating command (DEL, MGET, DBSIZE, fan-outs, …).
    /// Builds the per-shard target list, registers a pending slot for the
    /// aggregator, then dispatches each target (locally exec or cross-core
    /// send).
    pub(crate) fn start_multi<A: ArgvView + ?Sized>(
        &mut self,
        conn_id: u64,
        seq: u64,
        args: &A,
        route: Route,
        is_quit: bool,
    ) {
        let (targets, agg) = self.build_multi_targets(args, route);
        let remaining = targets.len().max(1) as u32;
        self.push_pending_slot(conn_id, remaining, agg, is_quit);
        // An empty key set (shouldn't happen given routing) still resolves.
        if targets.is_empty() {
            self.fold(conn_id, seq, Part::Int(0));
            return;
        }
        self.dispatch_targets(conn_id, seq, targets);
    }

    /// Register a `PendingSlot` for `conn_id` waiting on `remaining` parts
    /// to fold via `agg`. Pushed in seq order, so the slot's index is
    /// `seq - next_emit`. Captures the conn's current `proto` so a
    /// later `materialize` (run when the last sub-reply lands) shapes
    /// the bytes per the proto that was in effect at dispatch time.
    pub(crate) fn push_pending_slot(&mut self, conn_id: u64, remaining: u32, agg: Agg, is_quit: bool) {
        if let Some(c) = self.conns.get_mut(&conn_id) {
            let proto = c.proto;
            c.pending.push_back(PendingSlot {
                remaining,
                agg,
                done: None,
                proto,
            });
            if is_quit {
                c.closing = true;
            }
        }
    }

    /// Fan a built target list out: locally exec on this shard, or send the
    /// unbatched `Inbound::Request` to the owning peer. Single-key forwards
    /// never come through here — `start_single` pushes them straight onto
    /// `request_batch` (the hot batched lane).
    pub(crate) fn dispatch_targets(&mut self, conn_id: u64, seq: u64, targets: Vec<(usize, Op)>) {
        for (shard, op) in targets {
            if shard == self.id {
                let part = self.exec_op(op);
                self.fold(conn_id, seq, part);
            } else {
                // Multi-key ops (Del/MSet/Gather/…) use the unbatched path.
                self.xshard_inflight += 1;
                self.send_to(
                    shard,
                    Inbound::Request {
                        origin: self.id,
                        conn: conn_id,
                        seq,
                        op,
                    },
                );
            }
        }
    }

    /// Flush each shard's accumulated single-key dispatch batch as one
    /// cross-core `RequestBatch`. Call once per reactor loop. The bitmap
    /// short-circuit early-returns when no shard has
    /// pending requests. An earlier attempt tried splitting the slow body into a
    /// `#[inline(never)]` helper and reverted — body is small enough
    /// that LLVM inlines it cleanly; forcing the outline added a fn
    /// call on the cross-shard hot path with no upside.
    #[inline]
    pub(crate) fn flush_requests(&mut self) {
        if self.request_batch_nonempty == 0 {
            return;
        }
        let mut mask = self.request_batch_nonempty;
        self.request_batch_nonempty = 0;
        while mask != 0 {
            let s = mask.trailing_zeros() as usize;
            mask &= mask - 1;
            if s == self.id || self.request_batch[s].is_empty() {
                continue;
            }
            let reqs = std::mem::take(&mut self.request_batch[s]);
            self.xshard_inflight += reqs.len() as u64;
            self.send_to(s, Inbound::RequestBatch { origin: self.id, reqs });
        }
    }

    // `build_multi_targets` / `group_keys` / `build_gather` / `fanout_keys` /
    // `build_mset_targets` live in [`crate::exec_build`] so this file stays
    // under the 500-LOC house rule; still on the same `impl Shard`.
    //
    // `exec_op` (the cross-shard request dispatcher) lives in
    // [`crate::exec_op`]; do_subscribe / do_publish / deliver_publish /
    // flush_publish live in [`crate::exec_pubsub`]. All still on the same
    // `impl Shard`, but split so this file stays under 500 LOC.

    /// Append a mutating command to this shard's AOF, if enabled (best-effort).
    pub(crate) fn log<A: ArgvView + ?Sized>(&mut self, args: &A) {
        if let Some(aof) = &mut self.aof
            && let Err(e) = aof.append(args)
        {
            eprintln!("kevy: shard {} aof append failed: {e}", self.id);
        }
    }

    /// Like [`Self::log`] but TTL-persistence-safe. After logging `args`, if
    /// it is a *relative*-TTL write (`EXPIRE`/`PEXPIRE`/`SETEX`/`PSETEX`/
    /// `SET … EX|PX`) it appends an absolute `PEXPIREAT key <unix_ms>` derived
    /// from the key's post-exec deadline. AOF replay re-anchors a relative TTL
    /// to restart-time — resetting every key to a fresh full TTL (a
    /// production incident root cause) — so the absolute follow-up overwrites that with the
    /// original wall-clock deadline. Already-absolute writes (`EXPIREAT`/
    /// `PEXPIREAT`) replay correctly and need no follow-up.
    pub(crate) fn log_write<A: ArgvView + ?Sized>(&mut self, args: &A) {
        self.log(args);
        // Hash field-TTL relative forms get the same absolute
        // follow-up discipline — `HPEXPIREAT key <abs> FIELDS …`
        // re-anchors the replay-time deadline to the original wall
        // clock. HPEXPIREAT itself is already absolute.
        if args
            .get(0)
            .is_some_and(|v| v.eq_ignore_ascii_case(b"HEXPIRE") || v.eq_ignore_ascii_case(b"HPEXPIRE"))
        {
            self.log_hash_ttl_followup(args);
            return;
        }
        if !relative_ttl_write(args) {
            return;
        }
        let Some(key) = args.get(1) else { return };
        let pttl = self.store.pttl(key);
        if pttl < 0 {
            return; // command left no live TTL (key gone / TTL cleared)
        }
        let abs = kevy_store::now_unix_ms().saturating_add(pttl as u64);
        let key = key.to_vec();
        let mut c = Argv::with_capacity(3, 0);
        c.push(b"PEXPIREAT");
        c.push(&key);
        c.push(abs.to_string().as_bytes());
        self.log(&c);
    }

    /// log_write helper: rewrite a relative `HEXPIRE`/`HPEXPIRE`
    /// frame's deadline as absolute unix-ms and append the canonical
    /// `HPEXPIREAT` follow-up (fields tail copied verbatim).
    fn log_hash_ttl_followup<A: ArgvView + ?Sized>(&mut self, args: &A) {
        if args.len() < 6 {
            return;
        }
        let Some(raw) = std::str::from_utf8(&args[2]).ok().and_then(|s| s.parse::<i64>().ok())
        else {
            return;
        };
        let ms = if args[0].eq_ignore_ascii_case(b"HEXPIRE") {
            raw.saturating_mul(1000)
        } else {
            raw
        };
        let abs = kevy_store::now_unix_ms().saturating_add_signed(ms);
        let mut c = Argv::with_capacity(args.len(), 0);
        c.push(b"HPEXPIREAT");
        c.push(&args[1]);
        c.push(abs.to_string().as_bytes());
        for i in 3..args.len() {
            c.push(&args[i]);
        }
        self.log(&c);
    }

    // `fold` (the seq-ordered result reducer) + `protocol_error` and the
    // `relative_ttl_write` / `decode_continuation` free fns live in
    // [`crate::exec_fold`] — same `impl<C: Commands> Shard<C>`, split out
    // so this file stays under the 500-LOC house rule.
}
