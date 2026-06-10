//! `WATCH` / `UNWATCH` dispatch and the `EXEC` pre-execution fan-out it
//! enables. Split out of [`crate::exec`] (which is already at the 500-LOC
//! house-rule cap) — every method here is on the same `impl Shard`.
//!
//! Design:
//! - `WATCH k1 k2 ...` groups keys by owning shard, fans out
//!   [`Op::CollectWatchVersions`], collects the returned `(key, version)`
//!   pairs into the connection's `watched` set (folded through
//!   [`Agg::WatchCollect`]), then replies `+OK`.
//! - `UNWATCH` clears the conn's watched set in place and replies `+OK`.
//! - `EXEC` with a non-empty watched set pre-allocates `N+1` reply slots
//!   (1 header + N queued-cmd placeholders), fans out
//!   [`Op::CheckWatch`] grouped by shard, and folds the dirty bit through
//!   [`Agg::ExecPrep`]. On dirty the header becomes `*-1\r\n` and the
//!   placeholders emit zero bytes; on clean the header becomes `*N\r\n`
//!   and each queued cmd is dispatched at its pre-allocated seq via
//!   `start_command_at_seq`.

use crate::message::{Agg, DispatchMeta, Inbound, Op, Part, PendingSlot, SmallReply};
use crate::reduce::{drain_front, shard_of};
use crate::shard::Shard;
use crate::{Commands, ResolvedCmd, Route};
use kevy_resp::{Argv, ArgvView, RespVersion, encode_array_len};
use std::collections::HashMap;

impl<C: Commands> Shard<C> {
    /// `WATCH key [key ...]` — group by owning shard, fan
    /// [`Op::CollectWatchVersions`] out, fold each shard's
    /// `(key, version)` reply into the conn's `watched` set + emit `+OK`.
    pub(crate) fn do_watch<A: ArgvView + ?Sized>(
        &mut self,
        conn_id: u64,
        seq: u64,
        args: &A,
    ) {
        let mut by_shard: HashMap<usize, Vec<Vec<u8>>> = HashMap::new();
        for i in 1..args.len() {
            let key = &args[i];
            by_shard
                .entry(shard_of(key, self.nshards))
                .or_default()
                .push(key.to_vec());
        }
        let targets: Vec<(usize, Op)> = by_shard
            .into_iter()
            .map(|(s, ks)| (s, Op::CollectWatchVersions(ks)))
            .collect();
        let remaining = targets.len().max(1) as u32;
        if let Some(c) = self.conns.get_mut(&conn_id) {
            let proto = c.proto;
            c.pending.push_back(PendingSlot {
                remaining,
                agg: Agg::WatchCollect { pairs: Vec::new() },
                done: None,
                proto,
            });
        }
        if targets.is_empty() {
            // Defensive — `Route::Watch` requires args.len() >= 2.
            self.fold(conn_id, seq, Part::WatchVersions(Vec::new()));
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

    /// `UNWATCH` — clear the conn's watched set in place and reply `+OK`.
    /// Local-only; no cross-shard work (the registry entries on owning
    /// shards stay until overwritten — see the `watch_versions` field
    /// doc in `kevy-store`).
    pub(crate) fn do_unwatch(&mut self, conn_id: u64, seq: u64) {
        if let Some(c) = self.conns.get_mut(&conn_id) {
            c.watched.clear();
            c.pending.push_back(PendingSlot {
                remaining: 1,
                agg: Agg::First(None),
                done: None,
                proto: c.proto,
            });
        }
        self.fold(conn_id, seq, Part::Reply(SmallReply::from_slice(b"+OK\r\n")));
    }

    /// `EXEC` with a non-empty `watched` set: pre-allocate `N+1` slots,
    /// fan [`Op::CheckWatch`] grouped by shard, stash queued cmds in
    /// [`Agg::ExecPrep`]. The fold path resumes in [`Self::finalize_watch_agg`].
    pub(crate) fn exec_transaction_watched(
        &mut self,
        conn_id: u64,
        queued: Vec<Argv>,
        watched: Vec<(Vec<u8>, u64)>,
    ) {
        let n = queued.len();
        let Some((header_seq, base_idx)) = self.preallocate_exec_slots(conn_id, queued) else {
            return;
        };
        let by_shard = self.group_watched_pairs(watched);
        let groups = by_shard.len().max(1) as u32;
        if let Some(c) = self.conns.get_mut(&conn_id)
            && let Some(slot) = c.pending.get_mut(base_idx)
        {
            slot.remaining = groups;
        }
        if by_shard.is_empty() {
            // Defensive — non-empty `watched` always groups into ≥1 entry.
            self.fold(conn_id, header_seq, Part::Int(0));
            return;
        }
        for (shard, pairs) in by_shard {
            self.send_check_watch(conn_id, header_seq, shard, pairs);
        }
        // `n` is implicit in the slot layout — keep it bound so future
        // edits that touch slot counts have a single source of truth.
        let _ = n;
    }

    /// Push the header slot + `queued.len()` placeholder slots into the
    /// conn's ring, advancing `next_seq` by `1 + queued.len()`. Returns
    /// `(header_seq, base_idx)` or `None` if the conn vanished.
    fn preallocate_exec_slots(
        &mut self,
        conn_id: u64,
        queued: Vec<Argv>,
    ) -> Option<(u64, usize)> {
        let n = queued.len();
        let c = self.conns.get_mut(&conn_id)?;
        let header_seq = c.next_seq;
        let base_idx = c.pending.len();
        let proto = c.proto;
        c.next_seq += 1 + n as u64;
        c.pending.push_back(PendingSlot {
            remaining: 1, // overwritten once we know the group count
            agg: Agg::ExecPrep { dirty: false, queued, header_seq },
            done: None,
            proto,
        });
        for _ in 0..n {
            c.pending.push_back(PendingSlot {
                remaining: 1,
                agg: Agg::First(None),
                done: None,
                proto: c.proto,
            });
        }
        Some((header_seq, base_idx))
    }

    /// Group `(key, version)` pairs by the key's owning shard.
    fn group_watched_pairs(
        &self,
        watched: Vec<(Vec<u8>, u64)>,
    ) -> HashMap<usize, Vec<(Vec<u8>, u64)>> {
        let mut by_shard: HashMap<usize, Vec<(Vec<u8>, u64)>> = HashMap::new();
        for (k, v) in watched {
            by_shard
                .entry(shard_of(&k, self.nshards))
                .or_default()
                .push((k, v));
        }
        by_shard
    }

    /// Run one shard's `CheckWatch` inline (local) or ship it cross-core.
    fn send_check_watch(
        &mut self,
        conn_id: u64,
        seq: u64,
        shard: usize,
        pairs: Vec<(Vec<u8>, u64)>,
    ) {
        let op = Op::CheckWatch(pairs);
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

    /// Called from [`Shard::fold`] when an `Agg::WatchCollect` /
    /// `Agg::ExecPrep` slot's last sub-reply has arrived. Handles the
    /// conn-state mutation + downstream dispatch that those accumulators
    /// can't express as a pure RESP materialisation.
    pub(crate) fn finalize_watch_agg(&mut self, conn_id: u64, seq: u64, agg: Agg) {
        match agg {
            Agg::WatchCollect { pairs } => self.finalize_watch_collect(conn_id, seq, pairs),
            Agg::ExecPrep {
                dirty,
                queued,
                header_seq,
            } => self.finalize_exec_prep(conn_id, header_seq, dirty, queued),
            // Unreachable — `fold` only sends WatchCollect/ExecPrep here.
            _ => {}
        }
    }

    /// `WATCH` completion: move the collated pairs into `conn.watched`,
    /// fill the slot's `done` with `+OK`, drain.
    fn finalize_watch_collect(
        &mut self,
        conn_id: u64,
        seq: u64,
        pairs: Vec<(Vec<u8>, u64)>,
    ) {
        let Some(c) = self.conns.get_mut(&conn_id) else { return };
        c.watched.extend(pairs);
        let idx = (seq - c.next_emit) as usize;
        if let Some(slot) = c.pending.get_mut(idx) {
            slot.done = Some(SmallReply::from_slice(b"+OK\r\n"));
        }
        drain_front(c);
    }

    /// `EXEC` pre-check completion. Dirty → header is `*-1`, every queued
    /// placeholder emits zero bytes. Clean → header is `*N`, then each
    /// queued cmd is dispatched at its pre-allocated seq.
    fn finalize_exec_prep(
        &mut self,
        conn_id: u64,
        header_seq: u64,
        dirty: bool,
        queued: Vec<Argv>,
    ) {
        let n = queued.len();
        if dirty {
            if let Some(c) = self.conns.get_mut(&conn_id) {
                let base_idx = (header_seq - c.next_emit) as usize;
                if let Some(h) = c.pending.get_mut(base_idx) {
                    h.done = Some(SmallReply::from_slice(b"*-1\r\n"));
                }
                for i in 0..n {
                    if let Some(p) = c.pending.get_mut(base_idx + 1 + i) {
                        p.done = Some(SmallReply::from_slice(b""));
                    }
                }
                drain_front(c);
            }
            return;
        }
        let mut header_bytes = Vec::with_capacity(8);
        encode_array_len(&mut header_bytes, n as i64);
        if let Some(c) = self.conns.get_mut(&conn_id) {
            let base_idx = (header_seq - c.next_emit) as usize;
            if let Some(h) = c.pending.get_mut(base_idx) {
                h.done = Some(SmallReply::from_vec(header_bytes));
            }
            drain_front(c);
        }
        // Each queued cmd's slot was pre-allocated in `exec_transaction_watched`.
        for (i, cmd) in queued.iter().enumerate() {
            let qseq = header_seq + 1 + i as u64;
            let resolved = self.commands.resolve(cmd);
            self.start_command_at_seq(conn_id, qseq, cmd, resolved);
        }
    }

    /// Like [`Shard::start_command`] but the `PendingSlot` at `seq` is
    /// already in the conn's ring (placeholder pushed by
    /// `exec_transaction_watched`). All routes funnel through `_at_seq`
    /// variants that mutate the existing slot rather than pushing a new
    /// one — otherwise the placeholder would leak and stall the conn.
    fn start_command_at_seq<A: ArgvView + ?Sized>(
        &mut self,
        conn_id: u64,
        seq: u64,
        args: &A,
        resolved: ResolvedCmd,
    ) {
        let ResolvedCmd { route, is_quit, is_write, wake_idx, .. } = resolved;
        match route {
            // Pub/sub + WATCH inside MULTI is rejected at queue time
            // (`handle_command` errors before queuing). UNWATCH inside MULTI
            // is a queued no-op: clear `watched` (already empty here — taken
            // at EXEC entry) and emit `+OK`.
            Route::Unwatch => self.fill_placeholder(conn_id, seq, b"+OK\r\n".to_vec()),
            Route::Subscribe
            | Route::Unsubscribe
            | Route::Psubscribe
            | Route::Punsubscribe
            | Route::Publish
            | Route::Watch
            | Route::Hello
            | Route::Rename { .. } => self.fill_placeholder(
                conn_id,
                seq,
                b"-ERR pub/sub or WATCH or HELLO or RENAME not allowed inside MULTI in v2-3a (queued-RENAME orchestration pending v2-3b)\r\n".to_vec(),
            ),
            Route::Local => {
                let meta = DispatchMeta { is_write, wake_idx, key_idx: None };
                self.start_single_at_seq(conn_id, seq, args, self.id, is_quit, meta)
            }
            Route::Single(idx) => {
                let shard = shard_of(&args[idx], self.nshards);
                let meta = DispatchMeta { is_write, wake_idx, key_idx: Some(idx as u8) };
                self.start_single_at_seq(conn_id, seq, args, shard, is_quit, meta)
            }
            other => self.start_multi_at_seq(conn_id, seq, args, other, is_quit),
        }
    }

    /// Fill a pre-allocated placeholder slot with literal RESP bytes and
    /// drain. Used by `start_command_at_seq` for the conn-level routes
    /// that don't make sense inside MULTI.
    fn fill_placeholder(&mut self, conn_id: u64, seq: u64, bytes: Vec<u8>) {
        let Some(c) = self.conns.get_mut(&conn_id) else { return };
        let idx = (seq - c.next_emit) as usize;
        if let Some(slot) = c.pending.get_mut(idx) {
            slot.done = Some(SmallReply::from_vec(bytes));
        }
        drain_front(c);
    }

    /// `start_single` for a pre-allocated slot — skip `push_pending_slot`
    /// and the in-order inline fast path (a placeholder always sits in
    /// front of us, so `seq != next_emit`).
    fn start_single_at_seq<A: ArgvView + ?Sized>(
        &mut self,
        conn_id: u64,
        seq: u64,
        args: &A,
        shard: usize,
        is_quit: bool,
        meta: DispatchMeta,
    ) {
        // EXEC's queued cmds inherit the conn's proto at execution time
        // (the proto is captured per-cmd at the forward site). If the conn
        // negotiated HELLO 3 before MULTI / between QUEUED frames, the
        // queued cmds also emit RESP3 shapes. AOF logging + WATCH bump
        // happen inside `exec_op`, driven by `meta`.
        let proto = self.conns.get(&conn_id).map_or(RespVersion::V2, |c| c.proto);
        if is_quit
            && let Some(c) = self.conns.get_mut(&conn_id)
        {
            c.closing = true;
        }
        if shard == self.id {
            let part = self.run_dispatch(args, proto, meta);
            self.fold(conn_id, seq, part);
        } else {
            let argv = self.argv_pool.take_filled(args);
            self.request_batch[shard].push((conn_id, seq, argv, proto, meta));
        }
    }

    /// `start_multi` for a pre-allocated slot — reconfigure the slot's
    /// `remaining` + `agg` to match the target list, then dispatch.
    fn start_multi_at_seq<A: ArgvView + ?Sized>(
        &mut self,
        conn_id: u64,
        seq: u64,
        args: &A,
        route: Route,
        is_quit: bool,
    ) {
        let (targets, agg) = self.build_multi_targets(args, route);
        let remaining = targets.len().max(1) as u32;
        if let Some(c) = self.conns.get_mut(&conn_id) {
            let idx = (seq - c.next_emit) as usize;
            if let Some(slot) = c.pending.get_mut(idx) {
                slot.remaining = remaining;
                slot.agg = agg;
            }
            if is_quit {
                c.closing = true;
            }
        }
        if targets.is_empty() {
            self.fold(conn_id, seq, Part::Int(0));
            return;
        }
        self.dispatch_targets(conn_id, seq, targets);
    }
}
