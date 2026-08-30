//! Target-side registry of cross-shard block waiters, split from
//! `block_xshard.rs` for the 500-LOC house rule.
//!
//! Owns the two waiter indexes and the serve escrow. The protocol that
//! drives it lives in `block_xshard.rs`; this file is only bookkeeping.

use crate::BlockKind;
use crate::block_xshard::XWaiter;
use kevy_resp::{Argv, RespVersion};
use std::collections::HashMap;

/// Target-side registry of cross-shard waiters, keyed by the watched key
/// (multiple origins may block on the same key) with an `(origin, conn)`
/// secondary index for O(1) cancel.
#[derive(Default)]
pub(crate) struct XShardWaiters {
    by_key: HashMap<Vec<u8>, Vec<XWaiter>>,
    by_conn: HashMap<(usize, u64), Vec<Vec<u8>>>,
    /// The undo for an in-flight serve, keyed by `(origin, conn)`.
    ///
    /// A serve pops here and the reply travels to the origin; until the
    /// origin says it reached a live client, this shard holds the
    /// command that would put the element back. Released on
    /// `BlockServeAck`, applied on `BlockServeAbort`. Bounded by the
    /// number of cross-shard serves in flight, which is bounded by the
    /// number of cross-shard-blocked conns.
    escrow: HashMap<(usize, u64), Argv>,
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
    pub(crate) fn arm(&mut self, key: &[u8], w: XWaiter) {
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
    pub(crate) fn waiters_on(&self, key: &[u8]) -> Vec<(usize, u64)> {
        self.by_key
            .get(key)
            .map(|q| q.iter().map(|w| (w.origin, w.conn)).collect())
            .unwrap_or_default()
    }

    /// The frozen replay command for `(origin, conn)` on `key`, if armed.
    pub(crate) fn serve_argv(
        &self,
        key: &[u8],
        origin: usize,
        conn: u64,
    ) -> Option<(Argv, RespVersion)> {
        self.by_key.get(key).and_then(|q| {
            q.iter()
                .find(|w| w.origin == origin && w.conn == conn)
                .map(|w| (w.serve_argv.clone(), w.proto))
        })
    }

    /// Hold the undo for an in-flight serve.
    pub(crate) fn escrow_put(&mut self, origin: usize, conn: u64, undo: Argv) {
        self.escrow.insert((origin, conn), undo);
    }

    /// Take the undo back out — `Some` only while a serve is unresolved.
    pub(crate) fn escrow_take(&mut self, origin: usize, conn: u64) -> Option<Argv> {
        self.escrow.remove(&(origin, conn))
    }

    /// The block kind of `(origin, conn)`'s waiter on `key` — needed to
    /// build the undo before serving.
    pub(crate) fn kind_of(&self, key: &[u8], origin: usize, conn: u64) -> Option<BlockKind> {
        self.by_key
            .get(key)
            .and_then(|q| q.iter().find(|w| w.origin == origin && w.conn == conn).map(|w| w.kind))
    }

    /// Drop every waiter for `(origin, conn)` across all its keys.
    pub(crate) fn drop_for(&mut self, origin: usize, conn: u64) {
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
