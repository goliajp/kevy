//! `PSUBSCRIBE` / `PUNSUBSCRIBE` + the pattern half of `PUBLISH` delivery.
//!
//! Split out of [`crate::exec_pubsub`] so the channel-precise fast path
//! (15.6 M msg/s, 2.28× valkey) stays uncluttered. Every method here is
//! still on the same `impl<C: Commands> Shard<C>`.
//!
//! Design:
//! - Per conn `psub: HashSet<Vec<u8>>` of subscribed patterns (in
//!   [`crate::conn::Conn`]). Disjoint from `sub` — a PUBLISH that matches
//!   both channel + a pattern yields one `message` AND one `pmessage`.
//! - Per shard `psub_local: HashMap<pattern, Vec<conn_id>>` for fast
//!   delivery — PUBLISH iterates this map's keys, glob_matches each
//!   against the channel, and delivers to the listed conns.
//! - Shared `pubsub_patterns: Vec<(pattern, count, shard_bitset)>` so
//!   PUBLISH knows which shards to fan out to + what receiver count to
//!   report. Walked linearly under a read lock; bit toggles happen only
//!   on shard-local count 0↔1 transitions so the bitset is exact (no
//!   over-approximation, unlike the channel registry's bit semantics).

use crate::Commands;
use crate::reduce::pubsub_pmessage;
use crate::shard::Shard;
use kevy_resp::{ArgvView, encode_array_len, encode_bulk, encode_integer, encode_null_bulk};
use kevy_store::glob_match;

impl<C: Commands> Shard<C> {
    /// `PSUBSCRIBE pattern [pattern ...]` — register each pattern on this
    /// shard + the shared pattern registry, emit per-pattern ack frames.
    /// Connection-level; never fans out to other shards.
    pub(crate) fn do_psubscribe<A: ArgvView + ?Sized>(
        &mut self,
        conn_id: u64,
        seq: u64,
        args: &A,
    ) {
        if self.conns.get(&conn_id).is_none() {
            return;
        }
        let patterns: Vec<Vec<u8>> = (1..args.len()).map(|i| args[i].to_vec()).collect();
        let (reply, changed) = self.apply_psub_to_conn(conn_id, &patterns, true);
        self.apply_psub_to_registry(&changed, true);
        self.fold_pubsub_reply(conn_id, seq, reply);
    }

    /// `PUNSUBSCRIBE [pattern ...]` — empty `patterns` removes every
    /// pattern this conn holds. Per-pattern ack frames mirror Redis;
    /// the "no patterns held + no args" case still emits one ack with a
    /// nil pattern slot and count 0.
    pub(crate) fn do_punsubscribe<A: ArgvView + ?Sized>(
        &mut self,
        conn_id: u64,
        seq: u64,
        args: &A,
    ) {
        let patterns: Vec<Vec<u8>> = match self.conns.get(&conn_id) {
            None => return,
            Some(_) if args.len() > 1 => (1..args.len()).map(|i| args[i].to_vec()).collect(),
            Some(c) => c.psub.iter().cloned().collect(),
        };
        let (reply, changed) = self.apply_psub_to_conn(conn_id, &patterns, false);
        self.apply_psub_to_registry(&changed, false);
        self.fold_pubsub_reply(conn_id, seq, reply);
    }

    /// Update the conn's `psub` set + this shard's `psub_local` table +
    /// build the per-pattern ack reply. Returns `(reply_bytes,
    /// patterns_that_actually_changed)` — only real transitions need to
    /// hit the shared registry (psub/punsub are idempotent).
    fn apply_psub_to_conn(
        &mut self,
        conn_id: u64,
        patterns: &[Vec<u8>],
        subscribe: bool,
    ) -> (Vec<u8>, Vec<Vec<u8>>) {
        let verb: &[u8] = if subscribe { b"psubscribe" } else { b"punsubscribe" };
        let mut out = Vec::new();
        let mut changed: Vec<Vec<u8>> = Vec::new();
        // The "no patterns to act on" edge case still gets one ack frame
        // with a nil pattern slot (matches Redis wire).
        if patterns.is_empty() {
            let count = self.psub_count_for(conn_id);
            encode_array_len(&mut out, 3);
            encode_bulk(&mut out, verb);
            encode_null_bulk(&mut out);
            encode_integer(&mut out, count as i64);
            return (out, changed);
        }
        for pat in patterns {
            let did = if subscribe {
                self.add_psub_local(conn_id, pat)
            } else {
                self.remove_psub_local(conn_id, pat)
            };
            if did {
                changed.push(pat.clone());
            }
            let count = self.psub_count_for(conn_id);
            encode_array_len(&mut out, 3);
            encode_bulk(&mut out, verb);
            encode_bulk(&mut out, pat);
            encode_integer(&mut out, count as i64);
        }
        (out, changed)
    }

    /// Add `pattern` to the conn's psub set + push `conn_id` onto this
    /// shard's `psub_local[pattern]`. Returns true iff the conn didn't
    /// already hold the pattern.
    fn add_psub_local(&mut self, conn_id: u64, pattern: &[u8]) -> bool {
        let Some(c) = self.conns.get_mut(&conn_id) else { return false };
        if !c.psub.insert(pattern.to_vec()) {
            return false;
        }
        self.psub_local
            .entry(pattern.to_vec())
            .or_default()
            .push(conn_id);
        true
    }

    /// Remove `pattern` from the conn's psub set + drop `conn_id` from
    /// this shard's `psub_local[pattern]`. Returns true iff the conn had
    /// actually held the pattern. Drops the local-table entry when the
    /// last subscriber leaves.
    fn remove_psub_local(&mut self, conn_id: u64, pattern: &[u8]) -> bool {
        let Some(c) = self.conns.get_mut(&conn_id) else { return false };
        if !c.psub.remove(pattern) {
            return false;
        }
        if let Some(ids) = self.psub_local.get_mut(pattern) {
            ids.retain(|&id| id != conn_id);
            if ids.is_empty() {
                self.psub_local.remove(pattern);
            }
        }
        true
    }

    /// Live channel + pattern subscription count for a conn (matches the
    /// integer reported in Redis's `(p)?subscribe` / `(p)?unsubscribe`
    /// ack frames).
    fn psub_count_for(&self, conn_id: u64) -> usize {
        match self.conns.get(&conn_id) {
            Some(c) => c.sub.len() + c.psub.len(),
            None => 0,
        }
    }

    /// Reflect each real psub/punsub transition into the shared pattern
    /// registry that PUBLISH consults. Bit toggles happen on local
    /// 0↔1 transitions only — exact, since each shard owns the entire
    /// life-cycle of its own subscribers.
    fn apply_psub_to_registry(&self, changed: &[Vec<u8>], subscribe: bool) {
        if changed.is_empty() {
            return;
        }
        let bit = 1u64 << self.id;
        let mut reg = self.pubsub_patterns.write().expect("pubsub patterns");
        for pat in changed {
            let pos = reg.iter().position(|(p, ..)| p == pat);
            if subscribe {
                let local_has_after = self
                    .psub_local
                    .get(pat)
                    .is_some_and(|ids| !ids.is_empty());
                match pos {
                    Some(i) => {
                        reg[i].1 += 1;
                        if local_has_after {
                            reg[i].2 |= bit;
                        }
                    }
                    None => reg.push((pat.clone(), 1, if local_has_after { bit } else { 0 })),
                }
            } else if let Some(i) = pos {
                reg[i].1 = reg[i].1.saturating_sub(1);
                // Clear our bit if our last local subscriber to `pat` is gone.
                let local_has_after = self
                    .psub_local
                    .get(pat)
                    .is_some_and(|ids| !ids.is_empty());
                if !local_has_after {
                    reg[i].2 &= !bit;
                }
                if reg[i].1 == 0 {
                    reg.swap_remove(i);
                }
            }
        }
    }

    /// Push a pre-built RESP reply blob onto this conn's pending ring at
    /// `seq` and fold it through immediately. Shared between
    /// `do_psubscribe` + `do_punsubscribe` so they don't reach into
    /// `crate::exec`'s slot-bookkeeping internals.
    fn fold_pubsub_reply(&mut self, conn_id: u64, seq: u64, reply: Vec<u8>) {
        if let Some(c) = self.conns.get_mut(&conn_id) {
            let proto = c.proto;
            c.pending.push_back(crate::message::PendingSlot {
                remaining: 1,
                agg: crate::message::Agg::First(None),
                done: None,
                proto,
            });
        }
        self.fold(conn_id, seq, crate::message::Part::Reply(reply));
    }

    /// Sum the receiver counts + OR the shard bitsets of every pattern
    /// in the shared registry that `glob_match`es `channel`. Returns
    /// `(0, 0)` when the registry is empty (the empty-Vec short-circuit
    /// is what protects the channel-only PUBLISH hot path).
    pub(crate) fn pattern_match_for_channel(&self, channel: &[u8]) -> (u32, u64) {
        let reg = self.pubsub_patterns.read().expect("pubsub patterns");
        if reg.is_empty() {
            return (0, 0);
        }
        let mut count: u32 = 0;
        let mut bits: u64 = 0;
        for (pat, cnt, b) in reg.iter() {
            if glob_match(pat, channel) {
                count = count.saturating_add(*cnt);
                bits |= *b;
            }
        }
        (count, bits)
    }

    /// Deliver a `pmessage` frame to every local conn whose `PSUBSCRIBE`d
    /// pattern matches `channel`. Empty-map short-circuit so channel-only
    /// workloads pay one HashMap::is_empty() check per local delivery.
    pub(crate) fn deliver_pmessages(&mut self, channel: &[u8], msg: &[u8]) {
        if self.psub_local.is_empty() {
            return;
        }
        // Walk patterns, glob-match each, collect (pattern, conn_id) pairs.
        // Two-pass to avoid borrowing `psub_local` while mutating `conns`.
        let mut plans: Vec<(Vec<u8>, u64)> = Vec::new();
        for (pat, ids) in &self.psub_local {
            if glob_match(pat, channel) {
                for id in ids {
                    plans.push((pat.clone(), *id));
                }
            }
        }
        if plans.is_empty() {
            return;
        }
        let mut touched: Vec<u64> = Vec::with_capacity(plans.len());
        for (pat, id) in plans {
            let frame = pubsub_pmessage(&pat, channel, msg);
            if let Some(c) = self.conns.get_mut(&id) {
                c.output.extend_from_slice(&frame);
                touched.push(id);
            }
        }
        self.dirty.extend_from_slice(&touched);
    }

    /// Drop a (closing) conn's patterns from this shard's `psub_local`
    /// AND the shared registry. Mirror of `unregister_subs` in
    /// [`crate::shard`] — called from `close_conn` so a gone subscriber
    /// stops contributing to PUBLISH counts + the fan-out bitset.
    pub(crate) fn unregister_psubs(&mut self, patterns: &std::collections::HashSet<Vec<u8>>) {
        if patterns.is_empty() {
            return;
        }
        // 1) Drop empty psub_local entries for any pattern the conn held
        //    AFTER the caller has already cleared `conn.psub`. We can't
        //    cross-reference the conn — it's been removed by close_conn —
        //    so we operate on the `patterns` snapshot the caller passed in.
        //
        //    (The actual `conn_id` removal from `psub_local[pat]` has
        //    already happened via `remove_psub_local` paths in the close
        //    sequence; here we just garbage-collect now-empty entries +
        //    the registry side. See `close_conn` in `crate::inbox`.)
        let bit = 1u64 << self.id;
        let mut reg = self.pubsub_patterns.write().expect("pubsub patterns");
        for pat in patterns {
            if let Some(i) = reg.iter().position(|(p, ..)| p == pat) {
                reg[i].1 = reg[i].1.saturating_sub(1);
                let local_has_after = self
                    .psub_local
                    .get(pat)
                    .is_some_and(|ids| !ids.is_empty());
                if !local_has_after {
                    reg[i].2 &= !bit;
                }
                if reg[i].1 == 0 {
                    reg.swap_remove(i);
                }
            }
        }
    }
}
