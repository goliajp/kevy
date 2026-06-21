//! Pub/sub fast-path methods on `Shard`. `SUBSCRIBE` / `UNSUBSCRIBE` /
//! `PUBLISH` and the per-loop publish-batch flush. Split out so [`crate::exec`]
//! stays under the 500-LOC house rule; everything is still on the same
//! `impl<C: Commands> Shard<C>`.

use crate::Commands;
use crate::message::{Agg, Inbound, Part, PendingSlot};
use crate::reduce::pubsub_message;
use crate::shard::Shard;
use kevy_resp::{
    ArgvView, RespVersion, encode_array_len, encode_bulk, encode_integer, encode_null_bulk,
    encode_push_header,
};

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
        let Some((reply, changed)) = self.apply_sub_to_conn(conn_id, &channels, subscribe, verb)
        else {
            return;
        };
        self.apply_sub_to_registry(&changed, subscribe);
        if let Some(c) = self.conns.get_mut(&conn_id) {
            c.pending.push_back(PendingSlot {
                remaining: 1,
                agg: Agg::First(None),
                done: None,
                proto: c.proto,
            });
        }
        self.fold(conn_id, seq, Part::Reply(crate::message::SmallReply::from_vec(reply)));
    }

    /// Update the connection's local subscription set + build the
    /// per-channel RESP reply. Returns `None` if the conn vanished
    /// mid-flight, else `(reply_bytes, channels_that_actually_changed)`.
    /// "Changed" matters because sub/unsub is idempotent — only real
    /// transitions must update the shared registry.
    fn apply_sub_to_conn(
        &mut self,
        conn_id: u64,
        channels: &[Vec<u8>],
        subscribe: bool,
        verb: &[u8],
    ) -> Option<(Vec<u8>, Vec<Vec<u8>>)> {
        let c = self.conns.get_mut(&conn_id)?;
        let proto = c.proto;
        let mut out = Vec::new();
        let mut changed: Vec<Vec<u8>> = Vec::new();
        // Header shape: V2 = `*3\r\n` array, V3 = `>3\r\n` push frame.
        // Per-ack header so a V3 conn that runs through this multiple
        // times keeps the push-frame demux contract.
        let emit_header = |out: &mut Vec<u8>, proto: RespVersion| match proto {
            RespVersion::V2 => encode_array_len(out, 3),
            RespVersion::V3 => encode_push_header(out, 3),
        };
        if channels.is_empty() {
            emit_header(&mut out, proto);
            encode_bulk(&mut out, verb);
            encode_null_bulk(&mut out);
            encode_integer(&mut out, c.sub.len() as i64);
        }
        for ch in channels {
            let did = if subscribe {
                c.sub.insert(ch.clone())
            } else {
                c.sub.remove(ch)
            };
            if did {
                changed.push(ch.clone());
            }
            emit_header(&mut out, proto);
            encode_bulk(&mut out, verb);
            encode_bulk(&mut out, ch);
            encode_integer(&mut out, c.sub.len() as i64);
        }
        Some((out, changed))
    }

    /// Reflect a real (sub/unsub) transition into the cross-shard
    /// registry that `PUBLISH` consults to route messages to the
    /// shards that actually hold subscribers.
    fn apply_sub_to_registry(&self, changed: &[Vec<u8>], subscribe: bool) {
        if changed.is_empty() {
            return;
        }
        let bit = 1u64 << self.id;
        let mut reg = self.pubsub.write().expect("pubsub registry");
        for ch in changed {
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

    /// PUBLISH: reply with the receiver count read **locally** from the shared
    /// registry (channel-precise + pattern matches; no cross-shard
    /// aggregation), then deliver the message fire-and-forget to exactly the
    /// shards that hold a subscriber (in parallel; no replies fold back).
    /// Replaces the old all-shards SumInt fan-out, which cost ~2N cross-core
    /// ops per publish (N sends + N replies).
    pub(crate) fn do_publish<A: ArgvView + ?Sized>(
        &mut self,
        conn_id: u64,
        seq: u64,
        args: &A,
    ) {
        let (mut count, mut bits) = self
            .pubsub
            .read()
            .expect("pubsub registry")
            .get(&args[1])
            .copied()
            .unwrap_or((0, 0));
        // Pattern path: walk the shared pattern registry and OR any matching
        // entry's count + bits in. Read-locked + linear; empty-Vec
        // short-circuit keeps channel-only PUBLISH undisturbed by the
        // existence of the registry.
        let (pcount, pbits) = self.pattern_match_for_channel(&args[1]);
        count = count.saturating_add(pcount);
        bits |= pbits;

        let mut reply = Vec::new();
        encode_integer(&mut reply, i64::from(count));
        if let Some(c) = self.conns.get_mut(&conn_id) {
            c.pending.push_back(PendingSlot {
                remaining: 1,
                agg: Agg::First(None),
                done: None,
                proto: c.proto,
            });
        }
        self.fold(conn_id, seq, Part::Reply(crate::message::SmallReply::from_vec(reply)));

        if bits != 0 {
            // H1.A (v1.25): when nshards==1 (every `--threads 1` deployment),
            // the cross-shard fan-out path is dead code — skip the
            // Arc::new + 2 × to_vec allocation and deliver inline from the
            // borrowed argv slices. Saves ~0.18 µs/publish at subs=10 (per
            // .claude/notes/v125-deco-axis-h-pubsub-edges.md sub-Q1).
            if self.nshards == 1 {
                debug_assert!(bits == 1 && self.id == 0);
                self.deliver_publish(&args[1], &args[2]);
            } else {
                // Multi-shard: share one payload across target shards (Arc, no
                // per-target byte clone). Deliver locally inline; queue remote
                // shards into per-target batches flushed once per drain (see
                // `flush_publish`).
                let m = std::sync::Arc::new((args[1].to_vec(), args[2].to_vec()));
                for s in 0..self.nshards {
                    if bits & (1u64 << s) == 0 {
                        continue;
                    }
                    if s == self.id {
                        self.deliver_publish(&m.0, &m.1);
                    } else {
                        self.publish_batch[s].push(m.clone());
                        self.publish_batch_nonempty |= 1u64 << s;
                    }
                }
            }
        }
    }

    /// Append a pub/sub message to every local subscriber of `channel`
    /// (channel-precise frame) and every local `PSUBSCRIBE`r whose
    /// pattern matches (pmessage frame). Marks all touched conns dirty so
    /// the reactor flushes them. Returns the channel-precise count.
    ///
    /// Mixed V2 + V3 subscribers on the same channel: V2 gets `*3`
    /// array frame, V3 gets `>3` push frame. Build both shapes
    /// upfront so the per-subscriber loop is one extend, not a full
    /// reencode (saves N alloc on a wide fan-out).
    pub(crate) fn deliver_publish(&mut self, channel: &[u8], msg: &[u8]) -> usize {
        let ids: Vec<u64> = self
            .conns
            .iter()
            .filter(|(_, c)| c.sub.contains(channel))
            .map(|(id, _)| *id)
            .collect();
        if !ids.is_empty() {
            let v2 = pubsub_message(channel, msg, kevy_resp::RespVersion::V2);
            let mut v3_cache: Option<Vec<u8>> = None;
            for id in &ids {
                if let Some(c) = self.conns.get_mut(id) {
                    let frame = match c.proto {
                        kevy_resp::RespVersion::V2 => &v2,
                        kevy_resp::RespVersion::V3 => {
                            v3_cache.get_or_insert_with(|| {
                                pubsub_message(channel, msg, kevy_resp::RespVersion::V3)
                            })
                        }
                    };
                    c.output.extend_from_slice(frame);
                }
            }
            self.dirty.extend_from_slice(&ids);
        }
        // Pattern path: defer to the pattern helper. Empty-map short-circuit
        // there too — channel-only workloads pay one `HashMap::is_empty`.
        self.deliver_pmessages(channel, msg);
        ids.len()
    }

    /// `HELLO [protover [AUTH user pass] [SETNAME name]]` — defer the
    /// reply shape + protocol-version decision to the embedder via
    /// [`crate::Commands::hello_reply`]. The runtime applies the
    /// returned proto version to the conn BEFORE folding the reply, so
    /// the ack itself goes out in the new proto's shape (a `HELLO 3`
    /// ack arrives as a RESP3 Map per the spec).
    pub(crate) fn do_hello<A: ArgvView + ?Sized>(
        &mut self,
        conn_id: u64,
        seq: u64,
        args: &A,
    ) {
        let current = match self.conns.get(&conn_id) {
            Some(c) => c.proto,
            None => return,
        };
        let (new_proto, reply) = self.commands.hello_reply(args, current);
        if let Some(c) = self.conns.get_mut(&conn_id) {
            c.proto = new_proto;
            c.pending.push_back(PendingSlot {
                remaining: 1,
                agg: Agg::First(None),
                done: None,
                proto: c.proto,
            });
        }
        self.fold(conn_id, seq, Part::Reply(crate::message::SmallReply::from_vec(reply)));
    }

    /// Flush each shard's accumulated pub/sub batch as one cross-core message —
    /// a flood of PUBLISHes costs one send per target shard per drain, not one
    /// per message. Call once per reactor loop iteration.
    #[inline]
    pub(crate) fn flush_publish(&mut self) {
        // E17 attempted same outline pattern as E15/E16 for this fn too
        // and reverted (see flush_requests revert note in exec.rs) —
        // body is small enough that LLVM already inlines it; forcing
        // the outline added a fn call on the cross-shard fan-out path.
        if self.publish_batch_nonempty == 0 {
            return;
        }
        let mut mask = self.publish_batch_nonempty;
        self.publish_batch_nonempty = 0;
        while mask != 0 {
            let s = mask.trailing_zeros() as usize;
            mask &= mask - 1;
            if s == self.id || self.publish_batch[s].is_empty() {
                continue;
            }
            let batch = std::mem::take(&mut self.publish_batch[s]);
            self.send_to(s, Inbound::DeliverPublish(batch));
        }
    }
}
