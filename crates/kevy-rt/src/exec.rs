//! Command execution: the half of [`Shard`] that turns parsed commands into
//! shard-local work and reduces the (possibly multi-shard) results.
//!
//! [`crate::shard`] owns the reactor (sockets, the inbound queue, flushing);
//! this module owns the *semantics* — transaction state, routing a command to
//! the shard(s) that own its keys, executing one op against the local store,
//! and folding sub-results into each connection's seq-ordered ring.

use crate::message::{
    Agg, GatherKind, Inbound, KeyShape, KvPairs, MultiOp, Op, Part, PendingSlot,
};
use crate::reduce::{drain_front, materialize, shard_of};
use crate::shard::Shard;
use crate::{Commands, ResolvedCmd, Route, TxnKind};
use kevy_resp::{ArgvView, encode_array_len};
use std::collections::HashMap;

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
                if let Some(c) = self.conns.get_mut(&conn_id) {
                    c.multi = None;
                }
                self.immediate_reply(conn_id, b"+OK\r\n".to_vec());
            }
            (true, TxnKind::Exec) => self.exec_transaction(conn_id),
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
            c.pending.push_back(PendingSlot {
                remaining: 1,
                agg: Agg::First(None),
                done: None,
            });
        }
        self.fold(conn_id, seq, Part::Reply(bytes));
    }

    /// `EXEC` — emit a `*N` array header, then run the queued commands in order.
    /// The seq-ordered ring concatenates their replies into one valid array.
    /// (Single-machine: same-shard commands are atomic on their core; we do not
    /// add a global no-interleave lock across shards. WATCH is not yet supported.)
    fn exec_transaction(&mut self, conn_id: u64) {
        let queued = match self.conns.get_mut(&conn_id) {
            Some(c) => c.multi.take().unwrap_or_default(),
            None => return,
        };
        let mut header = Vec::new();
        encode_array_len(&mut header, queued.len() as i64);
        self.immediate_reply(conn_id, header);
        for cmd in &queued {
            let resolved = self.commands.resolve(cmd);
            self.start_command(conn_id, cmd, resolved);
        }
    }

    /// Assign a seq, fan the command out to the owning shard(s), fold local parts.
    fn start_command<A: ArgvView + ?Sized>(
        &mut self,
        conn_id: u64,
        args: &A,
        resolved: ResolvedCmd,
    ) {
        let seq = match self.conns.get_mut(&conn_id) {
            Some(c) => {
                let s = c.next_seq;
                c.next_seq += 1;
                s
            }
            None => return,
        };

        let is_quit = resolved.is_quit;
        let route = resolved.route;
        let is_write = resolved.is_write;
        // Connection-level pub/sub commands modify this conn directly.
        match route {
            Route::Subscribe => {
                self.do_subscribe(conn_id, seq, args, true);
                return;
            }
            Route::Unsubscribe => {
                self.do_subscribe(conn_id, seq, args, false);
                return;
            }
            Route::Publish => {
                self.do_publish(conn_id, seq, args);
                return;
            }
            _ => {}
        }

        // Fast path: a single-target command (keyless `Local` or single-key
        // `Single`) — the overwhelming majority (GET/SET/INCR/PING/…). Skip the
        // `Vec<(shard, Op)>` allocation + the aggregation fold loop entirely.
        let single = match route {
            Route::Local => Some(self.id),
            Route::Single(idx) => Some(shard_of(&args[idx], self.nshards)),
            _ => None,
        };
        if let Some(shard) = single {
            // In-order local fast path: the command runs on THIS shard and its
            // reply is the next to emit (nothing pending), so write it straight
            // into the connection's output — no PendingSlot, no fold, no reply
            // `Vec` alloc, no drain copy. (`seq == next_emit` here, so advancing
            // both `next_seq` (done above) and `next_emit` keeps them in step.)
            if shard == self.id
                && self.conns.get(&conn_id).is_some_and(|c| c.pending.is_empty())
            {
                if let Some(conn) = self.conns.get_mut(&conn_id) {
                    // Disjoint field borrows: commands / store / conn.output.
                    self.commands
                        .dispatch_into(&mut self.store, args, &mut conn.output);
                    conn.next_emit += 1;
                    if is_quit {
                        conn.closing = true;
                    }
                }
                if self.aof.is_some() && is_write {
                    self.log(args);
                }
                return;
            }
            if let Some(c) = self.conns.get_mut(&conn_id) {
                c.pending.push_back(PendingSlot {
                    remaining: 1,
                    agg: Agg::First(None),
                    done: None,
                });
                if is_quit {
                    c.closing = true;
                }
            }
            if shard == self.id {
                // Local-but-not-fast-path: only here we need an owned Argv to
                // hand to exec_op via Op::Dispatch.
                let part = self.exec_op(Op::Dispatch(args.to_argv()));
                self.fold(conn_id, seq, part);
            } else {
                // Cross-shard forward: materialise owned at the handoff. The
                // -c50 single-shard hot path never reaches here.
                self.request_batch[shard].push((conn_id, seq, args.to_argv()));
            }
            return;
        }

        // Multi-target / aggregating commands (DEL, MGET, DBSIZE, fan-outs, …).
        let (targets, agg): (Vec<(usize, Op)>, Agg) = match route {
            Route::Local | Route::Single(_) => unreachable!("handled by fast path"),
            Route::DelKeys => (self.group_keys(args, Op::Del), Agg::SumInt(0)),
            Route::ExistsKeys => (self.group_keys(args, Op::Exists), Agg::SumInt(0)),
            Route::Dbsize => (
                (0..self.nshards).map(|s| (s, Op::Dbsize)).collect(),
                Agg::SumInt(0),
            ),
            Route::Flush => (
                (0..self.nshards).map(|s| (s, Op::Flush)).collect(),
                Agg::AllOk,
            ),
            Route::Save => (
                (0..self.nshards).map(|s| (s, Op::Save)).collect(),
                Agg::AllOk,
            ),
            Route::RewriteAof => (
                (0..self.nshards).map(|s| (s, Op::RewriteAof)).collect(),
                Agg::AllOk,
            ),
            Route::MSet => {
                // args[1..] are key/value pairs; group by each key's shard.
                let mut by_shard: HashMap<usize, KvPairs> = HashMap::new();
                let mut i = 1;
                while i + 1 < args.len() {
                    by_shard
                        .entry(shard_of(&args[i], self.nshards))
                        .or_default()
                        .push((args[i].to_vec(), args[i + 1].to_vec()));
                    i += 2;
                }
                (
                    by_shard
                        .into_iter()
                        .map(|(s, p)| (s, Op::MSet(p)))
                        .collect(),
                    Agg::AllOk,
                )
            }
            Route::MGet => self.build_gather(args, GatherKind::Str, MultiOp::Mget),
            Route::SInter => self.build_gather(args, GatherKind::Set, MultiOp::SInter),
            Route::SUnion => self.build_gather(args, GatherKind::Set, MultiOp::SUnion),
            Route::SDiff => self.build_gather(args, GatherKind::Set, MultiOp::SDiff),
            Route::Keys(pat) => self.fanout_keys(pat, None, KeyShape::Keys),
            Route::Scan(pat) => self.fanout_keys(pat, None, KeyShape::Scan),
            Route::RandomKey => self.fanout_keys(None, Some(1), KeyShape::Random),
            // Handled above (early return).
            Route::Subscribe | Route::Unsubscribe | Route::Publish => unreachable!(),
        };

        let remaining = targets.len().max(1) as u32;
        if let Some(c) = self.conns.get_mut(&conn_id) {
            // Pushed in seq order, so this slot's index is `seq - next_emit`.
            c.pending.push_back(PendingSlot {
                remaining,
                agg,
                done: None,
            });
            if is_quit {
                c.closing = true;
            }
        }

        // An empty key set (shouldn't happen given routing) still resolves.
        if targets.is_empty() {
            self.fold(conn_id, seq, Part::Int(0));
            return;
        }
        for (shard, op) in targets {
            if shard == self.id {
                let part = self.exec_op(op);
                self.fold(conn_id, seq, part);
            } else if let Op::Dispatch(argv) = op {
                // Single-key command for a peer shard: batch it into one
                // cross-core send per target (flushed by `flush_requests`),
                // instead of one `Inbound::Request` per command. This is the
                // hot -c50 path; the ring/fold tax is what drags many shards
                // below single-shard throughput.
                self.request_batch[shard].push((conn_id, seq, argv));
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

    /// Group `args[1..]` keys by shard for a cross-shard gather.
    fn build_gather<A: ArgvView + ?Sized>(
        &self,
        args: &A,
        kind: GatherKind,
        op: MultiOp,
    ) -> (Vec<(usize, Op)>, Agg) {
        let keys: Vec<Vec<u8>> = (1..args.len()).map(|i| args[i].to_vec()).collect();
        let mut by_shard: HashMap<usize, Vec<Vec<u8>>> = HashMap::new();
        for k in &keys {
            by_shard
                .entry(shard_of(k, self.nshards))
                .or_default()
                .push(k.clone());
        }
        let targets = by_shard
            .into_iter()
            .map(|(s, ks)| (s, Op::Gather(kind, ks)))
            .collect();
        (
            targets,
            Agg::Gather {
                op,
                keys,
                got: HashMap::new(),
            },
        )
    }

    /// Fan a key-collection out to every shard (KEYS/SCAN/RANDOMKEY).
    fn fanout_keys(
        &self,
        pat: Option<Vec<u8>>,
        limit: Option<usize>,
        shape: KeyShape,
    ) -> (Vec<(usize, Op)>, Agg) {
        let targets = (0..self.nshards)
            .map(|s| (s, Op::CollectKeys(pat.clone(), limit)))
            .collect();
        (
            targets,
            Agg::Keys {
                shape,
                acc: Vec::new(),
            },
        )
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

    /// Split `args[1..]` (keys) by owning shard.
    fn group_keys<A: ArgvView + ?Sized>(
        &self,
        args: &A,
        mk: fn(Vec<Vec<u8>>) -> Op,
    ) -> Vec<(usize, Op)> {
        let mut by_shard: HashMap<usize, Vec<Vec<u8>>> = HashMap::new();
        for i in 1..args.len() {
            let key = &args[i];
            by_shard
                .entry(shard_of(key, self.nshards))
                .or_default()
                .push(key.to_vec());
        }
        by_shard
            .into_iter()
            .map(|(s, keys)| (s, mk(keys)))
            .collect()
    }

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
    pub(crate) fn fold(&mut self, conn_id: u64, seq: u64, part: Part) {
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
            _ => {}
        }
        slot.remaining -= 1;
        if slot.remaining == 0 {
            let agg = std::mem::replace(&mut slot.agg, Agg::AllOk);
            slot.done = Some(materialize(agg));
            drain_front(conn);
        }
    }

    pub(crate) fn protocol_error(&mut self, conn_id: u64) {
        let seq = match self.conns.get_mut(&conn_id) {
            Some(c) => {
                let s = c.next_seq;
                c.next_seq += 1;
                c.closing = true;
                c.pending.push_back(PendingSlot {
                    remaining: 1,
                    agg: Agg::First(None),
                    done: None,
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
