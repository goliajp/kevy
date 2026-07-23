//! Write-result resolution of a cross-shard block serve's escrow.
//!
//! When the origin serves a reply to a conn that appears alive, it does not
//! release the target's escrow immediately — a point-in-time "alive" can go
//! stale before the reply is actually written. Instead it records
//! `serve_confirm[conn] = target_shard` and lets the write outcome decide:
//! a clean flush on a live conn releases the escrow, a teardown (the FIN was
//! read, or a write errored) restores the element. Both reactors resolve by
//! the same two entry points here; split from `block_xshard` for the LOC cap.

use crate::Commands;
use crate::message::Inbound;
use crate::shard::Shard;

/// Debug-only signal that a cross-shard serve reply was processed by the
/// origin (i.e. `origin_on_serve_resp` ran). The escrow regression uses it
/// to tell a genuine cross-shard placement from a co-located one: with N
/// shards a random key lands on the conn's own shard ~1/N of the time, and
/// that takes the LOCAL block path, not this cross-shard one — so the test
/// retries until it provably exercised the cross-shard code. Non-I/O, so it
/// does not perturb the timing.
#[cfg(debug_assertions)]
pub mod counters {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    pub static CROSS_SHARD_SERVES: AtomicU64 = AtomicU64::new(0);
    #[inline]
    pub fn note_cross_shard_serve() {
        CROSS_SHARD_SERVES.fetch_add(1, Relaxed);
    }
    /// Cross-shard serves processed since process start.
    pub fn cross_shard_serves() -> u64 {
        CROSS_SHARD_SERVES.load(Relaxed)
    }
}

impl<C: Commands> Shard<C> {
    /// The serve reply for `conn` reached the socket (its output flushed
    /// without error): release the escrow the target has been holding.
    /// Idempotent — a spurious flush after the confirm is a no-op.
    pub(crate) fn confirm_serve_delivered(&mut self, conn: u64) {
        if let Some(shard) = self.serve_confirm.remove(&conn) {
            if shard == self.id {
                self.target_release_escrow(self.id, conn);
            } else {
                self.send_to(shard, Inbound::BlockServeAck { origin: self.id, conn });
            }
        }
    }

    /// `conn` is being torn down with a serve reply still unconfirmed — the
    /// write never succeeded, so the element never reached a live client.
    /// Restore it. Idempotent. Called from the connection-close path.
    pub(crate) fn restore_serve_on_teardown(&mut self, conn: u64) {
        if let Some(shard) = self.serve_confirm.remove(&conn) {
            if shard == self.id {
                self.target_apply_escrow(self.id, conn);
            } else {
                self.send_to(shard, Inbound::BlockServeAbort { origin: self.id, conn });
            }
        }
    }

    /// io_uring twin of the poller's flush-conn escrow resolution: settle a
    /// cross-shard block serve by the write outcome. `closing` (the client's
    /// FIN was seen, or a write to it errored) → the reply never reached a
    /// live client, restore; a clean full drain on a live conn → it did,
    /// release. Gated on `serve_confirm` being non-empty, so the write hot
    /// path pays a length check. Idempotent — a later teardown restore is a
    /// no-op once this has resolved. Linux-only: the io_uring reactor is.
    #[cfg(target_os = "linux")]
    pub(crate) fn uring_resolve_serve(
        &mut self,
        cid: u64,
        io: &kevy_map::KevyMap<u64, crate::uring_conn::UringConn>,
    ) {
        if self.serve_confirm.is_empty() {
            return;
        }
        let uc = io.get(&cid);
        let closing =
            uc.is_none_or(|u| u.closing) || self.conns.get(&cid).is_none_or(|c| c.closing);
        let drained = uc.is_some_and(|u| u.write_buf.is_empty() && u.write_arcs.is_empty())
            && self.conns.get(&cid).is_some_and(|c| c.output.is_empty());
        if closing {
            self.restore_serve_on_teardown(cid);
        } else if drained {
            self.confirm_serve_delivered(cid);
        }
    }
}
