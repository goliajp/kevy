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
pub(crate) use crate::block_xshard_registry::XShardWaiters;
use crate::blocked::{BlockKind, encode_block_timeout, unix_now_ms};
use crate::message::Inbound;
use crate::shard::Shard;
use kevy_resp::{Argv, ArgvView, RespVersion};

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
    /// The client went away while `serving` was set. The record outlives
    /// the connection on purpose: the serve already popped, and dropping
    /// the record here is what used to lose the element.
    pub(crate) abandoned: bool,
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
pub(crate) struct XWaiter {
    pub(crate) origin: usize,
    pub(crate) conn: u64,
    pub(crate) kind: BlockKind,
    /// `$`-frozen replay command for this key (snapshotted at arm time).
    pub(crate) serve_argv: Argv,
    pub(crate) proto: RespVersion,
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
            .map(|(key, serve_argv)| OriginKey { shard: self.shard_of(&key), key, serve_argv })
            .collect();
        if let Some(conn) = self.conns.get_mut(&conn_id) {
            conn.blocked = true;
        }
        let arms: Vec<(usize, Vec<u8>, Argv)> =
            keys.iter().map(|k| (k.shard, k.key.clone(), k.serve_argv.clone())).collect();
        self.origin_blocks.insert(
            conn_id,
            OriginBlock { kind, deadline_ms, proto, serving: false, abandoned: false, keys },
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

        if self.serve_via_list_move(conn, key) {
            return;
        }

        if shard == self.id {
            let reply = self.target_serve(self.id, conn, key);
            self.origin_on_serve_resp(conn, key.to_vec(), reply);
        } else {
            self.send_to(
                shard,
                Inbound::BlockServeReq { origin: self.id, conn, key: key.to_vec() },
            );
        }
    }

    /// A parked BRPOPLPUSH whose destination lives on another shard cannot be
    /// served by a local dispatch on the source's shard: `rpoplpush` would push
    /// the element into THAT shard's keyspace, where no reader of the
    /// destination will ever look. That is how this silently lost 9 of 12
    /// elements on an 8-shard server. Run the cross-shard orchestrator instead
    /// — it takes from the source's shard, pushes to the destination's,
    /// restores on WRONGTYPE, and hands the reply back through
    /// `origin_on_serve_resp`.
    ///
    /// Returns `true` when it took the serve.
    fn serve_via_list_move(&mut self, conn: u64, key: &[u8]) -> bool {
        let Some((src, dst)) = self.brpoplpush_pair(conn, key) else {
            return false;
        };
        if self.shard_of(&dst) == self.shard_of(&src) {
            return false;
        }
        // `fold` addresses a pending slot by `seq - conn.next_emit`, so the
        // orchestrator's slot must be handed the seq it will actually sit at.
        // Passing a bare 0 makes `fold` decide the reply was already emitted
        // and drop it on the floor — the Take lands, the chain stops, and the
        // element is stranded off both lists.
        let Some(seq) = self.conns.get(&conn).map(|c| c.next_emit + c.pending.len() as u64) else {
            return false;
        };
        self.start_list_move_inner(conn, seq, &src, &dst, false, true, true);
        true
    }

    /// `(source, destination)` when this conn is parked on a BRPOPLPUSH whose
    /// serve replay is `BRPOPLPUSH src dst 0`. `None` for every other kind.
    fn brpoplpush_pair(&self, conn: u64, key: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
        let ob = self.origin_blocks.get(&conn)?;
        if ob.kind != BlockKind::Brpoplpush {
            return None;
        }
        let ok = ob.keys.iter().find(|k| k.key == key)?;
        let dst = ok.serve_argv.get(2)?.to_vec();
        Some((key.to_vec(), dst))
    }

    /// origin: the serve result is back. Non-empty → deliver + unpark + cancel
    /// the rest. Empty (raced) → re-arm every key and keep waiting.
    pub(crate) fn origin_on_serve_resp(&mut self, conn: u64, key: Vec<u8>, reply: Vec<u8>) {
        #[cfg(debug_assertions)]
        crate::block_xshard_confirm::counters::note_cross_shard_serve();
        let Some(ob) = self.origin_blocks.get_mut(&conn) else {
            self.restore_serve_for_gone_record(conn, &key, &reply);
            return;
        };
        if reply.is_empty() {
            // Raced empty (key drained between ready and serve): nothing was
            // popped. Re-arm and keep waiting, unless the disconnect already
            // abandoned this — then just finish its deferred teardown.
            ob.serving = false;
            if ob.abandoned {
                if let Some(ob) = self.origin_blocks.remove(&conn) {
                    self.broadcast_cancel(conn, &ob.keys);
                }
                return;
            }
            self.rearm_all(conn);
            return;
        }
        // Fast path: the disconnect is already processed, or the kernel
        // already reports the peer gone — restore now, skip a doomed reply.
        let gone = ob.abandoned || self.conns.get(&conn).is_none_or(|c| c.sock.peer_gone());
        if gone {
            self.abort_serve(conn, &key);
            return;
        }
        // Appears alive, but "appears" is point-in-time. Do NOT release the
        // escrow now: deliver, record the target shard, and let the write
        // result decide (`resolve_serve_by_write`) — release on a clean flush
        // to a live conn, restore on teardown. See block_xshard_confirm.
        let target_shard = self.serve_shard_of(conn, &key);
        self.deliver_block(conn, reply);
        match target_shard {
            Some(shard) => {
                self.serve_confirm.insert(conn, shard);
            }
            None => self.abort_serve(conn, &key), // unreachable for a real serve
        }
    }

    /// origin: the serve could not be delivered — have the target apply
    /// the undo, then finish the teardown the disconnect deferred.
    fn abort_serve(&mut self, conn: u64, key: &[u8]) {
        if let Some(shard) = self.serve_shard_of(conn, key) {
            if shard == self.id {
                self.target_apply_escrow(self.id, conn);
            } else {
                self.send_to(shard, Inbound::BlockServeAbort { origin: self.id, conn });
            }
        }
        if let Some(ob) = self.origin_blocks.remove(&conn) {
            self.broadcast_cancel(conn, &ob.keys);
        }
    }

    /// Which shard served `key` for this conn.
    fn serve_shard_of(&self, conn: u64, key: &[u8]) -> Option<usize> {
        let ob = self.origin_blocks.get(&conn)?;
        ob.keys.iter().find(|k| k.key == key).map(|k| k.shard)
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
        let arms: Vec<(usize, Vec<u8>, Argv)> =
            ob.keys.iter().map(|k| (k.shard, k.key.clone(), k.serve_argv.clone())).collect();
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
        // A serve in flight has already popped on the target. Tearing the
        // record down here is what used to drop the reply on the floor and
        // lose the element -- so mark it and let `origin_on_serve_resp`
        // resolve it, exactly as the timeout sweep already does.
        if let Some(ob) = self.origin_blocks.get_mut(&conn)
            && ob.serving
        {
            ob.abandoned = true;
            return;
        }
        if let Some(ob) = self.origin_blocks.remove(&conn) {
            self.broadcast_cancel(conn, &ob.keys);
        }
    }

    /// Test seam: hold a cross-shard-serving conn's teardown so the serve
    /// reply is processed while the disconnect is still unnoticed — the
    /// exact load-induced ordering that lost an element (the reply reaches
    /// `origin_on_serve_resp` with `abandoned` false and the socket already
    /// dead). Deferring `close_conn` for such a conn keeps it present with
    /// `abandoned` false, so the peek-at-delivery guard is what has to catch
    /// it. Without a seam the window is real but load-only; this makes it
    /// certain, the honest way (per the existing serve-delay seam).
    ///
    /// Debug builds only, and only when the variable is set.
    #[cfg(debug_assertions)]
    pub(crate) fn hold_serving_close_for_tests(&self, conn: u64) -> bool {
        use std::sync::OnceLock;
        static ON: OnceLock<bool> = OnceLock::new();
        let on = *ON.get_or_init(|| std::env::var_os("KEVY_TEST_XSHARD_HOLD_CLOSE").is_some());
        on && self.origin_blocks.get(&conn).is_some_and(|ob| ob.serving && !ob.abandoned)
    }

    #[cfg(not(debug_assertions))]
    pub(crate) fn hold_serving_close_for_tests(&self, _conn: u64) -> bool {
        false
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
    keys.iter().map(|k| (k.clone(), commands.block_serve_argv(args, kind, k))).collect()
}
