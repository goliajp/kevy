//! Pub/sub fast-path methods on `Shard`. `SUBSCRIBE` / `UNSUBSCRIBE` /
//! `PUBLISH` and the per-loop publish-batch flush. Split out so [`crate::exec`]
//! stays under the 500-LOC house rule; everything is still on the same
//! `impl<C: Commands> Shard<C>`.

use crate::Commands;
use crate::message::{Agg, Inbound, Part, PendingSlot};
use crate::reduce::pubsub_message;
use crate::shard::Shard;
use kevy_resp::{ArgvView, encode_array_len, encode_bulk, encode_integer, encode_null_bulk};

impl<C: Commands> Shard<C> {
    pub(crate) fn do_subscribe<A: ArgvView + ?Sized>(
        &mut self,
        conn_id: u64,
        seq: u64,
        args: &A,
        subscribe: bool,
    ) {
        let verb: &[u8] = if subscribe {
            b"subscribe"
        } else {
            b"unsubscribe"
        };
        // Channels: the explicit args, or (UNSUBSCRIBE with none) all current subs.
        let channels: Vec<Vec<u8>> = match self.conns.get(&conn_id) {
            None => return,
            Some(_) if args.len() > 1 => (1..args.len()).map(|i| args[i].to_vec()).collect(),
            Some(c) => c.sub.iter().cloned().collect(),
        };
        // Track which channels actually changed (sub/unsub is idempotent) so the
        // shared registry count stays exact.
        let mut changed: Vec<Vec<u8>> = Vec::new();
        let reply = {
            let Some(c) = self.conns.get_mut(&conn_id) else {
                return;
            };
            let mut out = Vec::new();
            if channels.is_empty() {
                encode_array_len(&mut out, 3);
                encode_bulk(&mut out, verb);
                encode_null_bulk(&mut out);
                encode_integer(&mut out, c.sub.len() as i64);
            }
            for ch in &channels {
                let did = if subscribe {
                    c.sub.insert(ch.clone())
                } else {
                    c.sub.remove(ch)
                };
                if did {
                    changed.push(ch.clone());
                }
                encode_array_len(&mut out, 3);
                encode_bulk(&mut out, verb);
                encode_bulk(&mut out, ch);
                encode_integer(&mut out, c.sub.len() as i64);
            }
            out
        };
        // Reflect the change in the shared registry (PUBLISH reads it).
        if !changed.is_empty() {
            let bit = 1u64 << self.id;
            let mut reg = self.pubsub.write().expect("pubsub registry");
            for ch in &changed {
                if subscribe {
                    let e = reg.entry(ch.clone()).or_insert((0, 0));
                    e.0 += 1;
                    e.1 |= bit;
                } else {
                    let drop = match reg.get_mut(ch) {
                        Some(e) => {
                            e.0 = e.0.saturating_sub(1);
                            e.0 == 0
                        }
                        None => false,
                    };
                    if drop {
                        reg.remove(ch);
                    }
                }
            }
        }
        if let Some(c) = self.conns.get_mut(&conn_id) {
            c.pending.push_back(PendingSlot {
                remaining: 1,
                agg: Agg::First(None),
                done: None,
            });
        }
        self.fold(conn_id, seq, Part::Reply(reply));
    }

    /// PUBLISH: reply with the receiver count read **locally** from the shared
    /// registry (no cross-shard aggregation), then deliver the message
    /// fire-and-forget to exactly the shards that hold a subscriber (in
    /// parallel; no replies fold back). Replaces the old all-shards SumInt
    /// fan-out, which cost ~2N cross-core ops per publish (N sends + N replies).
    pub(crate) fn do_publish<A: ArgvView + ?Sized>(
        &mut self,
        conn_id: u64,
        seq: u64,
        args: &A,
    ) {
        let (count, bits) = self
            .pubsub
            .read()
            .expect("pubsub registry")
            .get(&args[1])
            .copied()
            .unwrap_or((0, 0));
        let mut reply = Vec::new();
        encode_integer(&mut reply, count as i64);
        if let Some(c) = self.conns.get_mut(&conn_id) {
            c.pending.push_back(PendingSlot {
                remaining: 1,
                agg: Agg::First(None),
                done: None,
            });
        }
        self.fold(conn_id, seq, Part::Reply(reply));

        if bits != 0 {
            // Share one payload across all target shards (Arc, no per-target byte
            // clone). Deliver locally inline; queue remote shards into per-target
            // batches flushed once per drain (see `flush_publish`).
            let m = std::sync::Arc::new((args[1].to_vec(), args[2].to_vec()));
            for s in 0..self.nshards {
                if bits & (1u64 << s) == 0 {
                    continue;
                }
                if s == self.id {
                    self.deliver_publish(&m.0, &m.1);
                } else {
                    self.publish_batch[s].push(m.clone());
                }
            }
        }
    }

    /// Append a pub/sub message to every local subscriber of `channel`; returns
    /// the count delivered and marks them dirty for the reactor to flush.
    pub(crate) fn deliver_publish(&mut self, channel: &[u8], msg: &[u8]) -> usize {
        let ids: Vec<u64> = self
            .conns
            .iter()
            .filter(|(_, c)| c.sub.contains(channel))
            .map(|(id, _)| *id)
            .collect();
        if ids.is_empty() {
            return 0;
        }
        let message = pubsub_message(channel, msg);
        for id in &ids {
            if let Some(c) = self.conns.get_mut(id) {
                c.output.extend_from_slice(&message);
            }
        }
        self.dirty.extend_from_slice(&ids);
        ids.len()
    }

    /// Flush each shard's accumulated pub/sub batch as one cross-core message —
    /// a flood of PUBLISHes costs one send per target shard per drain, not one
    /// per message. Call once per reactor loop iteration.
    #[inline]
    pub(crate) fn flush_publish(&mut self) {
        // Outer-empty short-circuit: the common hot path has no pub/sub.
        if self.publish_batch.iter().all(|b| b.is_empty()) {
            return;
        }
        for s in 0..self.nshards {
            if s == self.id || self.publish_batch[s].is_empty() {
                continue;
            }
            let batch = std::mem::take(&mut self.publish_batch[s]);
            self.send_to(s, Inbound::DeliverPublish(batch));
        }
    }
}
