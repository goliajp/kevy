//! `Shard::fold` — the seq-ordered result reducer — plus its small
//! free-fn helpers. Same `impl<C: Commands> Shard<C>` as [`crate::exec`];
//! split out so that file stays under the 500-LOC house rule.

use crate::Commands;
use crate::message::{Agg, Part, PendingSlot, SmallReply};
use crate::reduce::{drain_front, materialize};
use crate::shard::Shard;

impl<C: Commands> Shard<C> {
    /// Fold a sub-result into its slot; emit completed replies in seq order.
    /// The `WatchCollect` / `ExecPrep` accumulators don't materialise to RESP
    /// bytes — they hand off to [`crate::exec_watch`] for the conn-state
    /// mutation + downstream dispatch they require.
    // LOC-WAIVER: data-driven aggregation match table — one arm per
    // (Agg, Part) pairing + the finalize dispatch over orchestrator aggs.
    pub(crate) fn fold(&mut self, conn_id: u64, seq: u64, part: Part) {
        let watch_agg: Option<Agg> = {
            let Some(conn) =
                crate::conn::conn_at(&mut self.conns, &mut self.conn_slot_hint, conn_id)
            else {
                return;
            };
            if seq < conn.next_emit {
                return; // already emitted (defensive — shouldn't happen)
            }
            let idx = (seq - conn.next_emit) as usize;
            // The in-order single-target reply — the forwarded GET/SET of a
            // pipeline — goes straight to the output: no aggregator, no
            // stored copy, no materialise.
            let part = match part {
                Part::Reply(b)
                    if idx == 0
                        && matches!(
                            conn.pending.front(),
                            Some(PendingSlot { remaining: 1, agg: None, .. })
                        ) =>
                {
                    conn.output.extend_from_slice(b.bytes(&conn.parked));
                    conn.pending.pop_front();
                    conn.next_emit += 1;
                    drain_front(conn);
                    return;
                }
                part => part,
            };
            let Some(slot) = conn.pending.get_mut(idx) else {
                return;
            };
            let Some(agg) = slot.agg.as_deref_mut() else {
                // a single target: its reply is the slot's
                match part {
                    Part::Reply(b) if slot.remaining == 1 => slot.done = Some(b),
                    Part::Reply(b) => slot.agg = Some(Box::new(Agg::First(Some(b)))),
                    _ => {}
                }
                slot.remaining -= 1;
                if slot.remaining == 0 {
                    if slot.done.is_none() {
                        slot.done = Some(materialize(Agg::First(None), slot.proto));
                    }
                    drain_front(conn);
                }
                return;
            };
            // (Agg::AllOk, Part::Ok) `{}` body matches the catch-all `_ => {}`
            // body but documents the *expected* aggregator/part pairing — the
            // wildcard arm is the fallback for impossible combinations after
            // the dispatcher arms. match_same_arms would collapse the two and
            // hide the contract; keep them separate.
            #[allow(clippy::match_same_arms)]
            match (agg, part) {
                (Agg::First(dst), Part::Reply(b)) => *dst = Some(b),
                (Agg::SumInt(acc), Part::Int(n)) => *acc += n,
                // WAIT: reply = MIN over per-shard acked counts.
                (Agg::MinInt(acc), Part::Int(n)) => *acc = (*acc).min(n),
                // REPL.WAIT: every shard must report 1 (met).
                (Agg::ReplBarrier { ok, .. }, Part::Int(n)) => *ok &= n > 0,
                // REPL.TOKEN: pairs drop in by shard id.
                (Agg::ReplTokens { slots }, Part::ReplToken { shard, generation, next_offset }) => {
                    if let Some(s) = slots.get_mut(shard as usize) {
                        *s = Some((generation, next_offset));
                    }
                }
                (Agg::AllOk, Part::Ok) => {}
                (Agg::ExtensionGather { chunks, .. }, Part::ExtensionChunk(c)) => {
                    chunks.push(c);
                }
                (Agg::ClientList { text }, Part::ExtensionChunk(c)) => {
                    text.extend_from_slice(&c);
                }
                (Agg::ClientKill { killed, .. }, Part::Int(n)) => *killed += n,
                (Agg::Gather { got, .. }, Part::Gathered(items))
                | (Agg::ZStoreGather { got, .. }, Part::Gathered(items)) => {
                    for (k, g) in items {
                        got.insert(k, g);
                    }
                }
                // Geo *STORE step 1: the source shard's search result.
                (Agg::GeoStore { hits, .. }, Part::GeoHits(h)) => *hits = Some(h),
                (Agg::Keys { acc, .. }, Part::Keys(ks)) => acc.extend(ks),
                // Weighted reservoir: candidate i survives with probability
                // live_i / seen, which makes every key in the whole keyspace
                // exactly equally likely — a big shard's candidate wins more
                // often precisely because it stands for more keys. The shard's
                // own `draw` supplies the coin, so the fold stays a pure
                // (agg, part) function.
                // An empty shard's candidate is None — absorbed, weight zero.
                // It gets its own arm because the mismatch fallthrough below
                // treats an unpaired (agg, part) as a routing bug.
                (Agg::RandomKey { .. }, Part::RandomKey { key: None, .. }) => {}
                (Agg::RandomKey { key, seen }, Part::RandomKey { key: Some(k), live, draw }) => {
                    *seen += live.max(1);
                    if key.is_none() || kevy_rng_below(draw, *seen) < live.max(1) {
                        *key = Some(k);
                    }
                }
                // SCAN page: bank the keys, remember the shard's next
                // cursor, debit the COUNT work budget; the decision
                // (reply vs chain into the next shard) happens in
                // `finalize_scan_agg` once the slot completes.
                (
                    Agg::ScanPage { keys, next, budget, .. },
                    Part::ScanPage { next: n, keys: ks, visited },
                ) => {
                    keys.extend(ks);
                    *next = n;
                    *budget = budget.saturating_sub(visited);
                }
                (Agg::PrefixStats { keys, expires }, Part::PrefixStats { keys: k, expires: e }) => {
                    *keys += k;
                    *expires += e;
                }
                (Agg::SlowlogGet { entries, .. }, Part::SlowlogEntries(es)) => {
                    entries.extend(es);
                }
                (Agg::WatchCollect { pairs }, Part::WatchVersions(items)) => {
                    pairs.extend(items);
                }
                // Cross-shard XREAD gather: drop each stream's element into
                // its request-order slot.
                (Agg::XReadGather { slots }, Part::XReadElement { index, element })
                | (
                    Agg::XReadGroupCheck { refusals: slots, .. },
                    Part::XReadElement { index, element },
                ) => {
                    if let Some(slot) = slots.get_mut(index as usize) {
                        *slot = element;
                    }
                }
                (Agg::ExecPrep { dirty, .. }, Part::Int(n)) => *dirty |= n != 0,
                // Cross-shard RENAME orchestrator: buffer the step-1
                // result in the agg so finalize can ship step 2.
                (Agg::RenameOrchestrator { taken, .. }, Part::RenameTaken { value, ttl_ms }) => {
                    *taken = Some((value, ttl_ms))
                }
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
                (Agg::BitOpGather { got, .. }, Part::Gathered(pairs)) => {
                    got.extend(pairs);
                }
                // Cross-shard COPY: step 1's clone, then step 2's verdict.
                (Agg::CopyOrchestrator { read, .. }, Part::CopyRead(r)) => *read = Some(r),
                (Agg::CopyOrchestrator { stored, .. }, Part::CopyPutDone { stored: st }) => {
                    *stored = Some(st);
                }
                // Cross-shard list move: buffer each step's result in the agg
                // so finalize can decide the next hop.
                (Agg::ListMoveOrchestrator { taken, .. }, Part::ListMoveTaken(r)) => {
                    *taken = Some(r)
                }
                (Agg::ListMoveOrchestrator { pushed, .. }, Part::ListMovePushed { refused }) => {
                    *pushed = Some(refused.is_none())
                }
                // The terminal step-1 miss (RenameNoSuchSrc) leaves
                // `taken == None`; finalize reads that as "missing src".
                _ => {}
            }
            slot.remaining -= 1;
            if slot.remaining == 0 {
                let proto = slot.proto;
                let agg = slot.agg.take().map_or(Agg::First(None), |agg| *agg);
                if matches!(
                    agg,
                    Agg::WatchCollect { .. }
                        | Agg::ExecPrep { .. }
                        | Agg::RenameOrchestrator { .. }
                        | Agg::ListMoveOrchestrator { .. }
                        | Agg::CopyOrchestrator { .. }
                        | Agg::BitOpGather { .. }
                        | Agg::ZStoreGather { .. }
                        | Agg::GeoStore { .. }
                        | Agg::ExtensionGather { .. }
                        | Agg::ScanPage { .. }
                        | Agg::XReadGroupCheck { .. }
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
                    self.finalize_watch_agg(conn_id, seq, agg);
                }
                Agg::RenameOrchestrator { .. } => self.finalize_rename_agg(conn_id, seq, agg),
                Agg::ListMoveOrchestrator { .. } => self.finalize_list_move_agg(conn_id, seq, agg),
                Agg::CopyOrchestrator { .. } => self.finalize_copy_agg(conn_id, seq, agg),
                Agg::BitOpGather { .. } => self.finalize_bitop_agg(conn_id, seq, agg),
                Agg::ZStoreGather { .. } => self.finalize_zstore_agg(conn_id, seq, agg),
                Agg::GeoStore { .. } => self.finalize_geostore_agg(conn_id, seq, agg),
                Agg::ScanPage { .. } => self.finalize_scan_agg(conn_id, seq, agg),
                Agg::XReadGroupCheck { refusals, reads } => {
                    self.finalize_xread_check(conn_id, seq, refusals, reads);
                }
                Agg::ExtensionGather { argv, chunks } => {
                    let proto =
                        self.conns.get(&conn_id).map_or(kevy_resp::RespVersion::V2, |c| c.proto);
                    let reduced = self.commands.extension_reduce(&argv, chunks, proto);
                    if crate::propagation::is_armed() {
                        self.record_armed(&kevy_resp::Argv::from(argv.to_vec()));
                    }
                    match reduced {
                        crate::ExtensionReduced::Reply(reply) => {
                            self.fill_extension_slot(conn_id, seq, reply);
                        }
                        // Phase state rides inside the follow-up argv
                        // itself (stateless two-phase, no new agg
                        // variant).
                        crate::ExtensionReduced::Continue(argv2) => {
                            self.start_extension_phase(conn_id, seq, argv2);
                        }
                    }
                }
                // The match above is exhaustive over what fold ever puts
                // into `watch_agg` (only the orchestrator aggs). Anything
                // else is a bug; ignore so a stray slot doesn't crash
                // the reactor.
                _ => {}
            }
        }
    }

    pub(crate) fn protocol_error(&mut self, conn_id: u64) {
        self.mark_closing(conn_id);
        let seq = match self.conns.get_mut(&conn_id) {
            Some(c) => {
                let s = c.next_seq;
                c.next_seq += 1;
                let proto = c.proto;
                c.pending.push_back(PendingSlot { remaining: 1, agg: None, done: None, proto });
                s
            }
            None => return,
        };
        self.fold(conn_id, seq, Part::Reply(SmallReply::from_slice(b"-ERR Protocol error\r\n")));
    }
}

/// Uniform in `0..n` from one raw draw (Lemire's multiply-shift, no rejection).
/// The residual bias is under one part in 2^64/n — invisible at keyspace sizes.
fn kevy_rng_below(draw: u64, n: u64) -> u64 {
    if n <= 1 {
        return 0;
    }
    ((u128::from(draw) * u128::from(n)) >> 64) as u64
}
