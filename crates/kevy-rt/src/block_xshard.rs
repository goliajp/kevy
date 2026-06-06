//! Cross-shard BLOCK arbiter — the path for a `BLPOP` / `BRPOP` /
//! `XREAD BLOCK` / `XREADGROUP BLOCK` whose watched keys are not all on the
//! conn's own shard (a single remote key, or any multi-key form). The
//! single-key-on-this-shard case stays on the in-shard fast path
//! ([`crate::blocked::BlockedClients`]); this module is untouched by it.
//!
//! # Why an arbiter
//!
//! The conn parks on its **origin** shard (the one that owns the socket and
//! the per-conn reply ordering). The keys live on **target** shards. The
//! naive design — target pops on a write and ships the value to the origin —
//! loses data: if two watched keys go ready at once, both targets pop, but
//! the origin can only deliver one reply (the conn is woken once), so the
//! other popped value is dropped.
//!
//! So the origin is the **sole arbiter** and no target ever pops on its own
//! initiative:
//!
//! 1. **arm** — origin fans [`Inbound::BlockArm`] to each key's owning shard;
//!    each target registers a waiter and, if the key already has data, sends
//!    [`Inbound::BlockReady`] back.
//! 2. **ready** — a target's `LPUSH` / `XADD` to a watched key also sends
//!    [`Inbound::BlockReady`]. Still no pop.
//! 3. **serve** — the origin picks one ready key, marks the conn *serving*,
//!    and sends [`Inbound::BlockServeReq`]; only now does the target pop /
//!    consume and return the reply via [`Inbound::BlockServeResp`].
//! 4. **deliver / re-arm** — non-empty reply → origin writes it, unparks,
//!    broadcasts [`Inbound::BlockCancel`]. Empty reply (another client
//!    drained the key in the ready→serve window) → origin re-arms and waits.
//!
//! A key owned by the origin shard itself is handled inline (no message —
//! there is no self-ring), so a multi-key command that mixes local and
//! remote keys is one uniform code path.

use crate::Commands;
use crate::blocked::{BlockKind, encode_block_timeout, unix_now_ms};
use crate::message::Inbound;
use crate::reduce::shard_of;
use crate::shard::Shard;
use kevy_resp::{Argv, ArgvView, RespVersion};
use std::collections::HashMap;

/// Origin-side record for one cross-shard-blocked conn. Lives on the conn's
/// own shard, the sole arbiter of which ready key serves it.
pub(crate) struct OriginBlock {
    pub(crate) kind: BlockKind,
    /// Unix-ms deadline; `u64::MAX` = block forever.
    pub(crate) deadline_ms: u64,
    pub(crate) proto: RespVersion,
    /// A serve round-trip is in flight. Suppresses a second concurrent serve
    /// AND the timeout sweep, so a serve that pops data is never discarded by
    /// a timeout firing in the same window.
    pub(crate) serving: bool,
    pub(crate) keys: Vec<OriginKey>,
}

/// One watched key of an [`OriginBlock`]: its owning shard and the
/// single-key replay command (`$` still literal — frozen on the target).
pub(crate) struct OriginKey {
    pub(crate) key: Vec<u8>,
    pub(crate) shard: usize,
    pub(crate) serve_argv: Argv,
}

/// One target-side waiter: a (possibly remote) conn watching a key this
/// shard owns. Separate from [`crate::blocked::BlockedClients`] so the hot
/// single-key-local path pays nothing for this feature.
struct XWaiter {
    origin: usize,
    conn: u64,
    kind: BlockKind,
    /// `$`-frozen replay command for this key (snapshotted at arm time).
    serve_argv: Argv,
    proto: RespVersion,
}

/// Target-side registry of cross-shard waiters, keyed by the watched key
/// (multiple origins may block on the same key) with an `(origin, conn)`
/// secondary index for O(1) cancel.
#[derive(Default)]
pub(crate) struct XShardWaiters {
    by_key: HashMap<Vec<u8>, Vec<XWaiter>>,
    by_conn: HashMap<(usize, u64), Vec<Vec<u8>>>,
}

impl XShardWaiters {
    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }

    #[inline]
    pub(crate) fn is_watched(&self, key: &[u8]) -> bool {
        self.by_key.contains_key(key)
    }

    /// Register (or refresh, on a re-arm) a waiter for `(origin, conn)` on
    /// `key`. Idempotent: a re-arm replaces the existing entry's frozen
    /// `serve_argv` rather than appending a duplicate.
    fn arm(&mut self, key: &[u8], w: XWaiter) {
        let id = (w.origin, w.conn);
        let q = self.by_key.entry(key.to_vec()).or_default();
        if let Some(slot) = q.iter_mut().find(|e| (e.origin, e.conn) == id) {
            slot.serve_argv = w.serve_argv;
            slot.proto = w.proto;
            slot.kind = w.kind;
        } else {
            q.push(w);
            self.by_conn.entry(id).or_default().push(key.to_vec());
        }
    }

    /// Every `(origin, conn)` watching `key`, in registration (FIFO) order.
    fn waiters_on(&self, key: &[u8]) -> Vec<(usize, u64)> {
        self.by_key
            .get(key)
            .map(|q| q.iter().map(|w| (w.origin, w.conn)).collect())
            .unwrap_or_default()
    }

    /// The frozen replay command for `(origin, conn)` on `key`, if armed.
    fn serve_argv(&self, key: &[u8], origin: usize, conn: u64) -> Option<(Argv, RespVersion)> {
        self.by_key.get(key).and_then(|q| {
            q.iter()
                .find(|w| w.origin == origin && w.conn == conn)
                .map(|w| (w.serve_argv.clone(), w.proto))
        })
    }

    /// Drop every waiter for `(origin, conn)` across all its keys.
    fn drop_for(&mut self, origin: usize, conn: u64) {
        let Some(keys) = self.by_conn.remove(&(origin, conn)) else {
            return;
        };
        for key in keys {
            if let Some(q) = self.by_key.get_mut(&key) {
                q.retain(|w| !(w.origin == origin && w.conn == conn));
                if q.is_empty() {
                    self.by_key.remove(&key);
                }
            }
        }
    }
}

impl<C: Commands> Shard<C> {
    // ───────────────────────── origin side ─────────────────────────

    /// Park `conn` across shards: record the [`OriginBlock`] and arm every
    /// watched key on its owning shard. `entries` is `(key, serve_argv)` per
    /// watched key, `serve_argv` already narrowed to that one key (`$` still
    /// literal — the target freezes it). Used for a single remote key or any
    /// multi-key form.
    pub(crate) fn park_blocked_xshard(
        &mut self,
        conn_id: u64,
        kind: BlockKind,
        entries: Vec<(Vec<u8>, Argv)>,
        deadline_ms: u64,
        proto: RespVersion,
    ) {
        let keys: Vec<OriginKey> = entries
            .into_iter()
            .map(|(key, serve_argv)| OriginKey {
                shard: shard_of(&key, self.nshards),
                key,
                serve_argv,
            })
            .collect();
        if let Some(conn) = self.conns.get_mut(&conn_id) {
            conn.blocked = true;
        }
        let arms: Vec<(usize, Vec<u8>, Argv)> = keys
            .iter()
            .map(|k| (k.shard, k.key.clone(), k.serve_argv.clone()))
            .collect();
        self.origin_blocks.insert(
            conn_id,
            OriginBlock { kind, deadline_ms, proto, serving: false, keys },
        );
        self.arm_and_maybe_serve(conn_id, kind, proto, arms);
    }

    /// Arm every key then serve from a locally-ready one. Two phases so all
    /// `BlockArm`s are queued before any `BlockCancel` a synchronous local
    /// serve would emit (else a remote target could get its cancel before
    /// its arm and leak a waiter). Shared by park and re-arm.
    fn arm_and_maybe_serve(
        &mut self,
        conn: u64,
        kind: BlockKind,
        proto: RespVersion,
        arms: Vec<(usize, Vec<u8>, Argv)>,
    ) {
        let mut local_ready: Vec<Vec<u8>> = Vec::new();
        for (shard, key, serve_argv) in arms {
            if shard == self.id {
                if self.target_register(self.id, conn, &key, kind, serve_argv, proto) {
                    local_ready.push(key);
                }
            } else {
                self.send_to(
                    shard,
                    Inbound::BlockArm { origin: self.id, conn, key, kind, serve_argv, proto },
                );
            }
        }
        for key in local_ready {
            if !self.origin_blocks.contains_key(&conn) {
                break;
            }
            self.origin_on_ready(conn, &key);
        }
    }

    /// origin: a watched `key` may satisfy `conn`. Arbitrate: ignore if the
    /// conn is gone or already serving; otherwise begin a serve on `key`.
    pub(crate) fn origin_on_ready(&mut self, conn: u64, key: &[u8]) {
        let Some(ob) = self.origin_blocks.get_mut(&conn) else {
            return;
        };
        if ob.serving {
            return;
        }
        let Some(shard) = ob.keys.iter().find(|k| k.key == key).map(|k| k.shard) else {
            return; // not a key we're watching for this conn (stale)
        };
        ob.serving = true;
        if shard == self.id {
            let reply = self.target_serve(self.id, conn, key);
            self.origin_on_serve_resp(conn, key.to_vec(), reply);
        } else {
            self.send_to(
                shard,
                Inbound::BlockServeReq {
                    origin: self.id,
                    conn,
                    key: key.to_vec(),
                },
            );
        }
    }

    /// origin: the serve result is back. Non-empty → deliver + unpark + cancel
    /// the rest. Empty (raced) → re-arm every key and keep waiting.
    pub(crate) fn origin_on_serve_resp(&mut self, conn: u64, _key: Vec<u8>, reply: Vec<u8>) {
        let Some(ob) = self.origin_blocks.get_mut(&conn) else {
            return; // conn timed out / disconnected during the serve
        };
        if reply.is_empty() {
            ob.serving = false;
            self.rearm_all(conn);
            return;
        }
        self.deliver_block(conn, reply);
    }

    /// Write `reply` to the parked conn, unpark it, remove the origin record,
    /// and broadcast cancel to every target.
    fn deliver_block(&mut self, conn: u64, reply: Vec<u8>) {
        if let Some(c) = self.conns.get_mut(&conn) {
            c.blocked = false;
            c.output.extend_from_slice(&reply);
            c.next_emit += 1;
            self.dirty.push(conn);
        }
        if let Some(ob) = self.origin_blocks.remove(&conn) {
            self.broadcast_cancel(conn, &ob.keys);
        }
    }

    /// Re-arm every key after a raced-empty serve so each target re-checks
    /// readiness (idempotent on the target side — `XShardWaiters::arm`
    /// refreshes rather than duplicates).
    fn rearm_all(&mut self, conn: u64) {
        let Some(ob) = self.origin_blocks.get(&conn) else {
            return;
        };
        let proto = ob.proto;
        let kind = ob.kind;
        let arms: Vec<(usize, Vec<u8>, Argv)> = ob
            .keys
            .iter()
            .map(|k| (k.shard, k.key.clone(), k.serve_argv.clone()))
            .collect();
        self.arm_and_maybe_serve(conn, kind, proto, arms);
    }

    /// Send `BlockCancel` to each distinct target shard (inline for self).
    fn broadcast_cancel(&mut self, conn: u64, keys: &[OriginKey]) {
        let mut seen: Vec<usize> = Vec::new();
        for k in keys {
            if seen.contains(&k.shard) {
                continue;
            }
            seen.push(k.shard);
            if k.shard == self.id {
                self.xwaiters.drop_for(self.id, conn);
            } else {
                self.send_to(k.shard, Inbound::BlockCancel { origin: self.id, conn });
            }
        }
    }

    /// Periodic timeout sweep over origin-blocked conns. A conn currently
    /// `serving` is skipped (its in-flight serve resolves it). Fires one
    /// timeout reply per expired conn and broadcasts cancel.
    pub(crate) fn tick_xshard_timeouts(&mut self) {
        if self.origin_blocks.is_empty() {
            return;
        }
        let now = unix_now_ms();
        let expired: Vec<u64> = self
            .origin_blocks
            .iter()
            .filter(|(_, ob)| !ob.serving && ob.deadline_ms <= now)
            .map(|(&c, _)| c)
            .collect();
        for conn in expired {
            let Some(ob) = self.origin_blocks.remove(&conn) else {
                continue;
            };
            if let Some(c) = self.conns.get_mut(&conn) {
                c.blocked = false;
                encode_block_timeout(&mut c.output, ob.kind, ob.proto);
                c.next_emit += 1;
                self.dirty.push(conn);
            }
            self.broadcast_cancel(conn, &ob.keys);
        }
    }

    /// Disconnect cleanup: cancel a cross-shard-blocked conn's target
    /// registrations. Called from `close_conn` (origin side).
    pub(crate) fn cancel_xshard_on_close(&mut self, conn: u64) {
        if let Some(ob) = self.origin_blocks.remove(&conn) {
            self.broadcast_cancel(conn, &ob.keys);
        }
    }

    // ───────────────────────── target side ─────────────────────────

    /// target (remote-arm handler): register the waiter, then signal
    /// readiness if the key already has data. The origin-local arm path
    /// uses [`Self::target_register`] directly so it can defer the signal
    /// past the whole arm loop (see `park_blocked_xshard`).
    pub(crate) fn target_arm(
        &mut self,
        origin: usize,
        conn: u64,
        key: Vec<u8>,
        kind: BlockKind,
        serve_argv: Argv,
        proto: RespVersion,
    ) {
        if self.target_register(origin, conn, &key, kind, serve_argv, proto) {
            self.signal_ready(origin, conn, &key);
        }
    }

    /// target: register (or refresh, on re-arm) a waiter for `(origin,
    /// conn)` on `key`, freezing any `$` in `serve_argv` against this
    /// shard's live store. Returns whether the key already has data — the
    /// caller decides when to signal readiness.
    fn target_register(
        &mut self,
        origin: usize,
        conn: u64,
        key: &[u8],
        kind: BlockKind,
        serve_argv: Argv,
        proto: RespVersion,
    ) -> bool {
        let frozen = self
            .commands
            .resolve_block_argv(&mut self.store, &serve_argv, kind);
        let ready = self.commands.block_ready(&mut self.store, &frozen, kind);
        self.xwaiters.arm(
            key,
            XWaiter {
                origin,
                conn,
                kind,
                serve_argv: frozen,
                proto,
            },
        );
        ready
    }

    /// target: a write landed on `key` — signal every cross-shard waiter on
    /// it (each origin arbitrates). No pop here. Gated by the caller on
    /// `xwaiters.is_watched(key)`.
    pub(crate) fn target_wake_xshard(&mut self, key: &[u8]) {
        for (origin, conn) in self.xwaiters.waiters_on(key) {
            self.signal_ready(origin, conn, key);
        }
    }

    /// target → origin readiness signal (inline when origin is us).
    fn signal_ready(&mut self, origin: usize, conn: u64, key: &[u8]) {
        if origin == self.id {
            self.origin_on_ready(conn, key);
        } else {
            self.send_to(origin, Inbound::BlockReady { conn, key: key.to_vec() });
        }
    }

    /// target: serve `(origin, conn)`'s waiter on `key` — replay its frozen
    /// command (popping / consuming) and return the reply bytes. Empty =
    /// raced (key drained between ready and serve) → origin re-arms.
    pub(crate) fn target_serve(&mut self, origin: usize, conn: u64, key: &[u8]) -> Vec<u8> {
        let Some((argv, proto)) = self.xwaiters.serve_argv(key, origin, conn) else {
            return Vec::new();
        };
        let mut reply = Vec::new();
        match proto {
            RespVersion::V2 => self.commands.dispatch_into(&mut self.store, &argv, &mut reply),
            RespVersion::V3 => self
                .commands
                .dispatch_into_resp3(&mut self.store, &argv, &mut reply),
        }
        reply
    }

    /// target: drop all of `(origin, conn)`'s waiters (BlockCancel handler).
    pub(crate) fn target_cancel(&mut self, origin: usize, conn: u64) {
        self.xwaiters.drop_for(origin, conn);
    }
}

/// Build the per-key `(key, serve_argv)` list for a cross-shard park from
/// the original command. `serve_argv` is narrowed to one key via
/// [`Commands::block_serve_argv`]; `$` stays literal (frozen on the target).
pub(crate) fn build_serve_entries<C: Commands, A: ArgvView + ?Sized>(
    commands: &C,
    args: &A,
    kind: BlockKind,
    keys: &[Vec<u8>],
) -> Vec<(Vec<u8>, Argv)> {
    keys.iter()
        .map(|k| (k.clone(), commands.block_serve_argv(args, kind, k)))
        .collect()
}
