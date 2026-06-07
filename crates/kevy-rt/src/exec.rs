//! Command execution: the half of [`Shard`] that turns parsed commands into
//! shard-local work and reduces the (possibly multi-shard) results.
//!
//! [`crate::shard`] owns the reactor (sockets, the inbound queue, flushing);
//! this module owns the *semantics* — transaction state, routing a command to
//! the shard(s) that own its keys, executing one op against the local store,
//! and folding sub-results into each connection's seq-ordered ring.

use crate::message::{Agg, Inbound, Op, Part, PendingSlot};
use crate::reduce::{drain_front, materialize, shard_of};
use crate::shard::Shard;
use crate::{Commands, ResolvedCmd, Route, TxnKind};
use kevy_resp::{ArgvView, encode_array_len};

impl<C: Commands> Shard<C> {
    /// Apply transaction state (queue inside MULTI), else dispatch the command.
    pub(crate) fn handle_command<A: ArgvView + ?Sized>(&mut self, conn_id: u64, args: &A) {
        // One verb-resolution per cmd (was 4: txn_kind + route + is_quit +
        // is_write each scanned the verb separately). KevyCommands overrides
        // resolve() with a single match; non-overriding impls still pay 4×.
        let resolved = self.commands.resolve(args);
        let in_multi = self.conns.get(&conn_id).is_some_and(|c| c.multi.is_some());
        match (in_multi, &resolved.txn_kind) {
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
            (false, TxnKind::Watch) => self.start_command(conn_id, args, resolved),
            (true, TxnKind::Other) => {
                if let Some(q) = self.conns.get_mut(&conn_id).and_then(|c| c.multi.as_mut()) {
                    q.push(args.to_argv());
                }
                self.immediate_reply(conn_id, b"+QUEUED\r\n".to_vec());
            }
            (false, TxnKind::Other) => self.start_command(conn_id, args, resolved),
        }
    }

    /// Push a slot that resolves immediately to `bytes` (preserves seq order).
    fn immediate_reply(&mut self, conn_id: u64, bytes: Vec<u8>) {
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
        self.fold(conn_id, seq, Part::Reply(bytes));
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
            self.start_command(conn_id, cmd, resolved);
        }
    }

    /// Assign a seq, then hand off to the per-shape starter (pub/sub /
    /// single-target / multi-target). Each starter owns the rest of the
    /// command's life cycle: pending-slot bookkeeping, local exec, and
    /// cross-shard forwarding.
    fn start_command<A: ArgvView + ?Sized>(
        &mut self,
        conn_id: u64,
        args: &A,
        resolved: ResolvedCmd,
    ) {
        let Some(seq) = self.next_seq_for(conn_id) else { return };
        let ResolvedCmd {
            route,
            is_quit,
            is_write,
            block_hint,
            wake_idx,
            ..
        } = resolved;
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
            Route::Slowlog(sub) => self.start_slowlog(conn_id, seq, sub),
            Route::Local => {
                self.start_single(
                    conn_id, seq, args, self.id, is_quit, is_write, block_hint, wake_idx,
                );
            }
            Route::Single(idx) => {
                let shard = shard_of(&args[idx], self.nshards);
                self.start_single(
                    conn_id, seq, args, shard, is_quit, is_write, block_hint, wake_idx,
                );
            }
            // Multi-target / aggregating commands (DEL, MGET, DBSIZE, fan-outs, …).
            other => self.start_multi(conn_id, seq, args, other, is_quit),
        }
    }

    /// Reserve a `seq` for this command. `None` if the conn vanished between
    /// the parse loop and dispatch (rare; just drop the command).
    fn next_seq_for(&mut self, conn_id: u64) -> Option<u64> {
        let c = self.conns.get_mut(&conn_id)?;
        let s = c.next_seq;
        c.next_seq += 1;
        Some(s)
    }

    // `start_single` + `try_inline_local` (and their helpers `park_blocked`
    // / `post_write_housekeeping`) live in [`crate::exec_dispatch`] —
    // same `impl<C: Commands> Shard<C>`, split out so this file stays
    // under the 500-LOC house rule.

    /// Multi-target / aggregating command (DEL, MGET, DBSIZE, fan-outs, …).
    /// Builds the per-shard target list, registers a pending slot for the
    /// aggregator, then dispatches each target (locally exec or cross-core
    /// send).
    fn start_multi<A: ArgvView + ?Sized>(
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

    /// Fan a built target list out: locally exec on this shard, batch
    /// single-key forwards to peer shards (the hot -c50 path), and use the
    /// unbatched `Inbound::Request` for multi-key ops that don't fit the
    /// batch shape.
    pub(crate) fn dispatch_targets(&mut self, conn_id: u64, seq: u64, targets: Vec<(usize, Op)>) {
        for (shard, op) in targets {
            if shard == self.id {
                let part = self.exec_op(op);
                self.fold(conn_id, seq, part);
            } else if let Op::Dispatch(argv, proto) = op {
                // Single-key command for a peer shard: batch it into one
                // cross-core send per target (flushed by `flush_requests`),
                // instead of one `Inbound::Request` per command. This is the
                // hot -c50 path; the ring/fold tax is what drags many shards
                // below single-shard throughput.
                self.request_batch[shard].push((conn_id, seq, argv, proto));
            } else {
                // Multi-key ops (Del/MSet/Gather/…) keep the unbatched path.
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
    /// cross-core `RequestBatch` — a -c50 flood costs one send per target shard
    /// per loop, not one per command. Call once per reactor loop iteration.
    #[inline]
    pub(crate) fn flush_requests(&mut self) {
        // Outer-empty short-circuit: single-shard never has cross-shard reqs.
        if self.request_batch.iter().all(|b| b.is_empty()) {
            return;
        }
        for s in 0..self.nshards {
            if s == self.id || self.request_batch[s].is_empty() {
                continue;
            }
            let reqs = std::mem::take(&mut self.request_batch[s]);
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

    /// Fold a sub-result into its slot; emit completed replies in seq order.
    /// The `WatchCollect` / `ExecPrep` accumulators don't materialise to RESP
    /// bytes — they hand off to [`crate::exec_watch`] for the conn-state
    /// mutation + downstream dispatch they require.
    pub(crate) fn fold(&mut self, conn_id: u64, seq: u64, part: Part) {
        let watch_agg: Option<Agg> = {
            let Some(conn) = self.conns.get_mut(&conn_id) else {
                return;
            };
            if seq < conn.next_emit {
                return; // already emitted (defensive — shouldn't happen)
            }
            let idx = (seq - conn.next_emit) as usize;
            let Some(slot) = conn.pending.get_mut(idx) else {
                return;
            };
            match (&mut slot.agg, part) {
                (Agg::First(dst), Part::Reply(b)) => *dst = Some(b),
                (Agg::SumInt(acc), Part::Int(n)) => *acc += n,
                (Agg::AllOk, Part::Ok) => {}
                (Agg::Gather { got, .. }, Part::Gathered(items)) => {
                    for (k, g) in items {
                        got.insert(k, g);
                    }
                }
                (Agg::Keys { acc, .. }, Part::Keys(ks)) => acc.extend(ks),
                (Agg::SlowlogGet { entries, .. }, Part::SlowlogEntries(es)) => {
                    entries.extend(es);
                }
                (Agg::WatchCollect { pairs }, Part::WatchVersions(items)) => {
                    pairs.extend(items);
                }
                (Agg::ExecPrep { dirty, .. }, Part::Int(n)) => *dirty |= n != 0,
                // Cross-shard RENAME orchestrator: buffer the step-1
                // result in the agg so finalize can ship step 2.
                (
                    Agg::RenameOrchestrator { taken, .. },
                    Part::RenameTaken { value, ttl_ms },
                ) => *taken = Some((value, ttl_ms)),
                // Step 2's put result: `refused = None` → stored; `Some`
                // → NX-blocked, and the handed-back value lands in `taken`
                // so finalize can restore src before the `:0` reply.
                (
                    Agg::RenameOrchestrator { put_stored, taken, .. },
                    Part::RenamePutDone { refused },
                ) => {
                    *put_stored = Some(refused.is_none());
                    if refused.is_some() {
                        *taken = refused;
                    }
                }
                // The terminal step-1 miss (RenameNoSuchSrc) leaves
                // `taken == None`; finalize reads that as "missing src".
                _ => {}
            }
            slot.remaining -= 1;
            if slot.remaining == 0 {
                let proto = slot.proto;
                let agg = std::mem::replace(&mut slot.agg, Agg::AllOk);
                if matches!(
                    agg,
                    Agg::WatchCollect { .. }
                        | Agg::ExecPrep { .. }
                        | Agg::RenameOrchestrator { .. }
                ) {
                    Some(agg)
                } else {
                    slot.done = Some(materialize(agg, proto));
                    drain_front(conn);
                    None
                }
            } else {
                None
            }
        };
        if let Some(agg) = watch_agg {
            match agg {
                Agg::WatchCollect { .. } | Agg::ExecPrep { .. } => {
                    self.finalize_watch_agg(conn_id, seq, agg)
                }
                Agg::RenameOrchestrator { .. } => self.finalize_rename_agg(conn_id, seq, agg),
                // The match above is exhaustive over what fold ever puts
                // into `watch_agg` (only the orchestrator aggs). Anything
                // else is a bug; ignore so a stray slot doesn't crash
                // the reactor.
                _ => {}
            }
        }
    }

    pub(crate) fn protocol_error(&mut self, conn_id: u64) {
        let seq = match self.conns.get_mut(&conn_id) {
            Some(c) => {
                let s = c.next_seq;
                c.next_seq += 1;
                c.closing = true;
                let proto = c.proto;
                c.pending.push_back(PendingSlot {
                    remaining: 1,
                    agg: Agg::First(None),
                    done: None,
                    proto,
                });
                s
            }
            None => return,
        };
        self.fold(
            conn_id,
            seq,
            Part::Reply(b"-ERR Protocol error\r\n".to_vec()),
        );
    }
}
