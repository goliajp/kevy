//! Target-side half of the cross-shard block protocol, split from
//! `block_xshard.rs` for the 500-LOC house rule.
//!
//! The target owns the key: it arms waiters, signals readiness, runs the
//! serve, and holds the serve's undo in escrow until the origin says
//! whether the reply reached a live client.

use crate::block_xshard::XWaiter;
use crate::message::Inbound;
use crate::shard::Shard;
use crate::{BlockKind, Commands};
use kevy_resp::{Argv, RespVersion};

impl<C: Commands> Shard<C> {
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
    pub(crate) fn target_register(
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
        // Capture the undo BEFORE popping. The reply is about to leave
        // this shard, and if the origin's client is gone by the time it
        // arrives, this is the only thing that can put the element back
        // -- the origin holds RESP bytes, not an element.
        let kind = self.xwaiters.kind_of(key, origin, conn);
        if let Some(k) = kind
            && let Some(undo) = self.commands.block_restore_argv(&mut self.store, k, key)
        {
            self.xwaiters.escrow_put(origin, conn, undo);
        }
        serve_delay_for_tests();
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
    ///
    /// Deliberately leaves any escrow entry alone: cancel and the
    /// serve's ack/abort are independent messages, and a cancel that
    /// arrived first must not strand a popped element. The ack or abort
    /// is always sent, so the entry is always resolved.
    pub(crate) fn target_cancel(&mut self, origin: usize, conn: u64) {
        self.xwaiters.drop_for(origin, conn);
    }

    /// target: the origin delivered — the undo is no longer needed.
    pub(crate) fn target_release_escrow(&mut self, origin: usize, conn: u64) {
        self.xwaiters.escrow_take(origin, conn);
    }

    /// target: the origin could not deliver — put the element back by
    /// running the undo captured before the pop.
    pub(crate) fn target_apply_escrow(&mut self, origin: usize, conn: u64) {
        let Some(undo) = self.xwaiters.escrow_take(origin, conn) else {
            return;
        };
        let mut sink = Vec::new();
        self.commands.dispatch_into(&mut self.store, &undo, &mut sink);
        // The key has data again, so anyone else parked on it should be
        // told -- including waiters on this very shard.
        self.target_wake_xshard(&undo[1].to_vec());
    }
}

/// Test seam: widen the window between "the origin asked for a serve"
/// and "the reply gets back", so a disconnect can be landed inside it.
///
/// The cross-shard serve drop is unreachable from a test otherwise --
/// the race needs cancel propagation to lose to a push, which does not
/// happen on an unloaded machine, and a test that only sometimes
/// exercises the defect it guards is worse than one that clearly does
/// not. Widening the window is the honest way round: it makes the lossy
/// interleaving certain rather than making it rare enough to hide.
///
/// Debug builds only, and only when the variable is set, so it cannot
/// exist in a shipped binary.
#[cfg(debug_assertions)]
fn serve_delay_for_tests() {
    use std::sync::OnceLock;
    static MS: OnceLock<u64> = OnceLock::new();
    let ms = *MS.get_or_init(|| {
        std::env::var("KEVY_TEST_XSHARD_SERVE_DELAY_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    });
    if ms > 0 {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}

#[cfg(not(debug_assertions))]
fn serve_delay_for_tests() {}
