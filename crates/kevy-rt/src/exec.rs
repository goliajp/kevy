//! Command execution: the half of [`Shard`] that turns parsed commands into
//! shard-local work and reduces the (possibly multi-shard) results.
//!
//! [`crate::shard`] owns the reactor (sockets, the inbound queue, flushing);
//! this module owns the *semantics* — transaction state, routing a command to
//! the shard(s) that own its keys, executing one op against the local store,
//! and folding sub-results into each connection's seq-ordered ring.

use crate::message::{
    Agg, GatherKind, Gathered, Inbound, KeyShape, KvPairs, MultiOp, Op, Part, PendingSlot,
};
use crate::reduce::{drain_front, materialize, pubsub_message, shard_of};
use crate::shard::Shard;
use crate::{Commands, Route, TxnKind};
use kevy_persist::save_snapshot;
use kevy_resp::{encode_array_len, encode_bulk, encode_integer, encode_null_bulk};
use std::collections::HashMap;

impl<C: Commands> Shard<C> {
    /// Apply transaction state (queue inside MULTI), else dispatch the command.
    pub(crate) fn handle_command(&mut self, conn_id: u64, args: Vec<Vec<u8>>) {
        let kind = self.commands.txn_kind(&args);
        let in_multi = self.conns.get(&conn_id).is_some_and(|c| c.multi.is_some());
        match (in_multi, kind) {
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
                    q.push(args);
                }
                self.immediate_reply(conn_id, b"+QUEUED\r\n".to_vec());
            }
            (false, TxnKind::Other) => self.start_command(conn_id, args),
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
        for cmd in queued {
            self.start_command(conn_id, cmd);
        }
    }

    /// Assign a seq, fan the command out to the owning shard(s), fold local parts.
    fn start_command(&mut self, conn_id: u64, args: Vec<Vec<u8>>) {
        let seq = match self.conns.get_mut(&conn_id) {
            Some(c) => {
                let s = c.next_seq;
                c.next_seq += 1;
                s
            }
            None => return,
        };

        let is_quit = self.commands.is_quit(&args);
        let route = self.commands.route(&args);
        // Connection-level pub/sub commands modify this conn directly.
        match route {
            Route::Subscribe => {
                self.do_subscribe(conn_id, seq, &args, true);
                return;
            }
            Route::Unsubscribe => {
                self.do_subscribe(conn_id, seq, &args, false);
                return;
            }
            _ => {}
        }
        let (targets, agg): (Vec<(usize, Op)>, Agg) = match route {
            Route::Local => (vec![(self.id, Op::Dispatch(args))], Agg::First(None)),
            Route::Single(idx) => {
                let shard = shard_of(&args[idx], self.nshards);
                (vec![(shard, Op::Dispatch(args))], Agg::First(None))
            }
            Route::DelKeys => (self.group_keys(&args, Op::Del), Agg::SumInt(0)),
            Route::ExistsKeys => (self.group_keys(&args, Op::Exists), Agg::SumInt(0)),
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
            Route::MSet => {
                // args[1..] are key/value pairs; group by each key's shard.
                let mut by_shard: HashMap<usize, KvPairs> = HashMap::new();
                let mut i = 1;
                while i + 1 < args.len() {
                    by_shard
                        .entry(shard_of(&args[i], self.nshards))
                        .or_default()
                        .push((args[i].clone(), args[i + 1].clone()));
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
            Route::MGet => self.build_gather(&args, GatherKind::Str, MultiOp::Mget),
            Route::SInter => self.build_gather(&args, GatherKind::Set, MultiOp::SInter),
            Route::SUnion => self.build_gather(&args, GatherKind::Set, MultiOp::SUnion),
            Route::SDiff => self.build_gather(&args, GatherKind::Set, MultiOp::SDiff),
            Route::Keys(pat) => self.fanout_keys(pat, None, KeyShape::Keys),
            Route::Scan(pat) => self.fanout_keys(pat, None, KeyShape::Scan),
            Route::RandomKey => self.fanout_keys(None, Some(1), KeyShape::Random),
            Route::Publish => {
                let (channel, msg) = (args[1].clone(), args[2].clone());
                let targets = (0..self.nshards)
                    .map(|s| (s, Op::Publish(channel.clone(), msg.clone())))
                    .collect();
                (targets, Agg::SumInt(0))
            }
            // Handled above (early return).
            Route::Subscribe | Route::Unsubscribe => unreachable!(),
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
            } else {
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
    fn build_gather(
        &self,
        args: &[Vec<u8>],
        kind: GatherKind,
        op: MultiOp,
    ) -> (Vec<(usize, Op)>, Agg) {
        let keys: Vec<Vec<u8>> = args[1..].to_vec();
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

    /// Handle SUBSCRIBE/UNSUBSCRIBE: mutate this conn's subscription set and
    /// reply with one confirmation frame per channel (running count).
    fn do_subscribe(&mut self, conn_id: u64, seq: u64, args: &[Vec<u8>], subscribe: bool) {
        let verb: &[u8] = if subscribe {
            b"subscribe"
        } else {
            b"unsubscribe"
        };
        let reply = match self.conns.get_mut(&conn_id) {
            None => return,
            Some(c) => {
                // UNSUBSCRIBE with no channels means "all currently subscribed".
                let channels: Vec<Vec<u8>> = if args.len() > 1 {
                    args[1..].to_vec()
                } else {
                    c.sub.iter().cloned().collect()
                };
                let mut out = Vec::new();
                if channels.is_empty() {
                    encode_array_len(&mut out, 3);
                    encode_bulk(&mut out, verb);
                    encode_null_bulk(&mut out);
                    encode_integer(&mut out, c.sub.len() as i64);
                }
                for ch in &channels {
                    if subscribe {
                        c.sub.insert(ch.clone());
                    } else {
                        c.sub.remove(ch);
                    }
                    encode_array_len(&mut out, 3);
                    encode_bulk(&mut out, verb);
                    encode_bulk(&mut out, ch);
                    encode_integer(&mut out, c.sub.len() as i64);
                }
                out
            }
        };
        if let Some(c) = self.conns.get_mut(&conn_id) {
            c.pending.push_back(PendingSlot {
                remaining: 1,
                agg: Agg::First(None),
                done: None,
            });
        }
        self.fold(conn_id, seq, Part::Reply(reply));
    }

    /// Split `args[1..]` (keys) by owning shard.
    fn group_keys(&self, args: &[Vec<u8>], mk: fn(Vec<Vec<u8>>) -> Op) -> Vec<(usize, Op)> {
        let mut by_shard: HashMap<usize, Vec<Vec<u8>>> = HashMap::new();
        for key in &args[1..] {
            by_shard
                .entry(shard_of(key, self.nshards))
                .or_default()
                .push(key.clone());
        }
        by_shard
            .into_iter()
            .map(|(s, keys)| (s, mk(keys)))
            .collect()
    }

    /// Execute one op against this shard's store, logging mutations to the AOF.
    pub(crate) fn exec_op(&mut self, op: Op) -> Part {
        match op {
            Op::Dispatch(args) => {
                let reply = self.commands.dispatch(&mut self.store, &args);
                if self.commands.is_write(&args) {
                    self.log(&args);
                }
                Part::Reply(reply)
            }
            Op::Del(keys) => {
                let n = self.store.del(&keys);
                if n > 0 {
                    let mut c = Vec::with_capacity(keys.len() + 1);
                    c.push(b"DEL".to_vec());
                    c.extend(keys);
                    self.log(&c);
                }
                Part::Int(n as i64)
            }
            Op::Exists(keys) => Part::Int(self.store.exists(&keys) as i64),
            Op::Dbsize => Part::Int(self.store.dbsize() as i64),
            Op::Flush => {
                self.store.flush();
                self.log(&[b"FLUSHALL".to_vec()]);
                Part::Ok
            }
            Op::MSet(pairs) => {
                for (k, v) in &pairs {
                    self.store.set(k, v.clone(), None, false, false);
                }
                if !pairs.is_empty() {
                    let mut c = Vec::with_capacity(pairs.len() * 2 + 1);
                    c.push(b"MSET".to_vec());
                    for (k, v) in pairs {
                        c.push(k);
                        c.push(v);
                    }
                    self.log(&c);
                }
                Part::Ok
            }
            Op::Gather(kind, keys) => {
                let mut results = Vec::with_capacity(keys.len());
                for k in keys {
                    let g = match kind {
                        GatherKind::Str => {
                            Gathered::Str(self.store.get(&k).ok().flatten().map(|v| v.to_vec()))
                        }
                        GatherKind::Set => match self.store.set_snapshot(&k) {
                            Ok(members) => Gathered::Members(members),
                            Err(_) => Gathered::WrongType,
                        },
                    };
                    results.push((k, g));
                }
                Part::Gathered(results)
            }
            Op::CollectKeys(pat, limit) => {
                Part::Keys(self.store.collect_keys(pat.as_deref(), limit))
            }
            Op::Publish(channel, msg) => {
                let ids: Vec<u64> = self
                    .conns
                    .iter()
                    .filter(|(_, c)| c.sub.contains(&channel))
                    .map(|(id, _)| *id)
                    .collect();
                let count = ids.len();
                if count > 0 {
                    let message = pubsub_message(&channel, &msg);
                    for id in &ids {
                        if let Some(c) = self.conns.get_mut(id) {
                            c.output.extend_from_slice(&message);
                        }
                    }
                    // Mark subscribers dirty; the reactor flushes their output
                    // (epoll: flush_conn; io_uring: the arm loop submits a write
                    // for any conn with pending output — so delivery is batched
                    // through io_uring instead of a per-subscriber syscall).
                    self.dirty.extend_from_slice(&ids);
                }
                Part::Int(count as i64)
            }
            Op::Save => {
                let path = self.snapshot_path();
                match save_snapshot(&self.store, &path) {
                    // Snapshot now captures full state → reset the AOF.
                    Ok(()) => {
                        if let Some(aof) = &mut self.aof
                            && let Err(e) = aof.truncate()
                        {
                            eprintln!("kevy: shard {} aof truncate failed: {e}", self.id);
                        }
                    }
                    Err(e) => {
                        eprintln!(
                            "kevy: shard {} failed to save {}: {e}",
                            self.id,
                            path.display()
                        )
                    }
                }
                Part::Ok
            }
        }
    }

    /// Append a mutating command to this shard's AOF, if enabled (best-effort).
    fn log(&mut self, args: &[Vec<u8>]) {
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
