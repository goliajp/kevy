//! `Shard::fold` — the seq-ordered result reducer — plus its small
//! free-fn helpers. Same `impl<C: Commands> Shard<C>` as [`crate::exec`];
//! split out so that file stays under the 500-LOC house rule.

use crate::message::{Agg, Part, PendingSlot, SmallReply};
use crate::reduce::{drain_front, materialize};
use crate::shard::Shard;
use crate::Commands;
use kevy_resp::ArgvView;

impl<C: Commands> Shard<C> {
    /// Fold a sub-result into its slot; emit completed replies in seq order.
    /// The `WatchCollect` / `ExecPrep` accumulators don't materialise to RESP
    /// bytes — they hand off to [`crate::exec_watch`] for the conn-state
    /// mutation + downstream dispatch they require.
    // LOC-WAIVER: data-driven aggregation match table — one arm per
    // (Agg, Part) pairing + the finalize dispatch over orchestrator aggs.
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
            // (Agg::AllOk, Part::Ok) `{}` body matches the catch-all `_ => {}`
            // body but documents the *expected* aggregator/part pairing — the
            // wildcard arm is the fallback for impossible combinations after
            // the dispatcher arms. match_same_arms would collapse the two and
            // hide the contract; keep them separate.
            #[allow(clippy::match_same_arms)]
            match (&mut slot.agg, part) {
                (Agg::First(dst), Part::Reply(b)) => *dst = Some(b),
                (Agg::SumInt(acc), Part::Int(n)) => *acc += n,
                // WAIT: reply = MIN over per-shard acked counts.
                (Agg::MinInt(acc), Part::Int(n)) => *acc = (*acc).min(n),
                // REPL.WAIT: every shard must report 1 (met).
                (Agg::ReplBarrier { ok, .. }, Part::Int(n)) => *ok &= n > 0,
                // REPL.TOKEN: pairs drop in by shard id.
                (
                    Agg::ReplTokens { slots },
                    Part::ReplToken { shard, generation, next_offset },
                ) => {
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
                (Agg::Keys { acc, .. }, Part::Keys(ks)) => acc.extend(ks),
                (
                    Agg::PrefixStats { keys, expires },
                    Part::PrefixStats { keys: k, expires: e },
                ) => {
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
                (Agg::XReadGather { slots }, Part::XReadElement { index, element }) => {
                    if let Some(slot) = slots.get_mut(index as usize) {
                        *slot = element;
                    }
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
                        | Agg::ZStoreGather { .. }
                        | Agg::ExtensionGather { .. }
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
                Agg::ZStoreGather { .. } => self.finalize_zstore_agg(conn_id, seq, agg),
                Agg::ExtensionGather { argv, chunks } => {
                    let proto = self
                        .conns
                        .get(&conn_id)
                        .map_or(kevy_resp::RespVersion::V2, |c| c.proto);
                    match self.commands.extension_reduce(&argv, chunks, proto) {
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
            Part::Reply(SmallReply::from_slice(b"-ERR Protocol error\r\n")),
        );
    }
}

/// Does `args` set a TTL via a *relative* duration (vs absolute `*AT`)? Such
/// writes need an absolute `PEXPIREAT` follow-up in the AOF — see
/// [`Shard::log_write`]. `SET … EXAT|PXAT` aren't parsed by the server's SET,
/// so only `EX`/`PX` count here.
pub(crate) fn relative_ttl_write<A: ArgvView + ?Sized>(args: &A) -> bool {
    if args.len() < 3 {
        return false;
    }
    let verb = &args[0];
    if verb.eq_ignore_ascii_case(b"EXPIRE")
        || verb.eq_ignore_ascii_case(b"PEXPIRE")
        || verb.eq_ignore_ascii_case(b"SETEX")
        || verb.eq_ignore_ascii_case(b"PSETEX")
    {
        return true;
    }
    if verb.eq_ignore_ascii_case(b"SET") {
        return (3..args.len())
            .any(|i| args[i].eq_ignore_ascii_case(b"EX") || args[i].eq_ignore_ascii_case(b"PX"));
    }
    false
}
