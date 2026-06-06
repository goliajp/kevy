// The non-tick API (`add` / `pop_oldest_on_key` / `is_watched` / `BlockKind`
// variants) is consumed by the per-command wake-up wiring that lands in the
// follow-up sprints (BLPOP/BRPOP, XREAD BLOCK, XREADGROUP BLOCK). `expect`
// rather than `allow` so the warning re-fires automatically once those
// sprints connect callers — a missed wiring will not silently linger.
#![expect(dead_code, reason = "wired in v2-7d.2 / .3 / .4 sprint commits")]

//! Per-shard blocked-client registry, shared by `BLPOP` / `BRPOP` /
//! `XREAD BLOCK` / `XREADGROUP BLOCK`.
//!
//! Design: when a command blocks, the conn's `argv` + `proto` is stashed
//! under every key it watches. A subsequent write to any of those keys wakes
//! the oldest waiter (FIFO per key, matching Redis); a periodic tick sweeps
//! waiters past their `deadline_ms` and fires a nil reply.
//!
//! The registry holds no reactor / socket state — `Shard` owns the wake +
//! reply emission paths. `BlockedClients::pop_*` returns the bookkeeping;
//! the caller decides what RESP frame to write.

use crate::Commands;
use crate::shard::Shard;
use kevy_resp::{Argv, RespVersion};
use std::collections::{HashMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

/// Unix wall-clock milliseconds — the time base both the dispatcher (when
/// computing a waiter's `deadline_ms = now_ms + timeout_ms`) and the reactor
/// tick (when checking `deadline_ms <= now_ms`) read. System-time jumps
/// (NTP slew, manual clock change) are accepted: a backwards jump may make
/// a waiter expire late, but BLOCK is not a wall-clock contract.
#[inline]
pub(crate) fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Emit the RESP nil reply that a timed-out blocking command returns.
/// Shape depends on both proto and kind:
/// - RESP3: `_\r\n` (the null type) for all kinds.
/// - RESP2 `BLPOP` / `BRPOP`: nil array `*-1\r\n` (Redis returns nil array
///   so the multi-bulk reply slot stays well-typed).
/// - RESP2 `XREAD` / `XREADGROUP`: nil bulk `$-1\r\n` (matches "no streams
///   updated in this window" — also Redis's choice).
pub(crate) fn encode_block_timeout(out: &mut Vec<u8>, kind: BlockKind, proto: RespVersion) {
    match (proto, kind) {
        (RespVersion::V3, _) => out.extend_from_slice(b"_\r\n"),
        (RespVersion::V2, BlockKind::Blpop | BlockKind::Brpop) => {
            out.extend_from_slice(b"*-1\r\n")
        }
        (RespVersion::V2, BlockKind::XReadBlock | BlockKind::XReadGroupBlock) => {
            out.extend_from_slice(b"$-1\r\n")
        }
    }
}

/// Which blocking command a waiter is parked in. Drives both timeout-nil
/// shape and wake-retry dispatch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BlockKind {
    Blpop,
    Brpop,
    XReadBlock,
    XReadGroupBlock,
}

pub(crate) struct BlockedClient {
    pub(crate) conn_id: u64,
    pub(crate) kind: BlockKind,
    /// Unix-ms wall clock when this waiter expires. `u64::MAX` = block forever.
    pub(crate) deadline_ms: u64,
    pub(crate) argv: Argv,
    pub(crate) proto: RespVersion,
}

/// FIFO per key; secondary index by conn for O(1) cleanup on wake / close.
#[derive(Default)]
pub(crate) struct BlockedClients {
    by_key: HashMap<Vec<u8>, VecDeque<BlockedClient>>,
    by_conn: HashMap<u64, Vec<Vec<u8>>>,
}

impl BlockedClients {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Was a write on `key` watched by any blocker? `is_empty()` short-circuit
    /// keeps the hot push/xadd path free of map lookups when nothing's parked.
    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }

    #[inline]
    pub(crate) fn is_watched(&self, key: &[u8]) -> bool {
        self.by_key.contains_key(key)
    }

    /// Register one waiter on each of `keys`. The same waiter is cloned into
    /// every key's FIFO; the wake path drops the surviving copies via
    /// `drop_for_conn` once any one fires (so a multi-key BLPOP woken by key
    /// A does not also fire on a later push to key B).
    pub(crate) fn add(
        &mut self,
        conn_id: u64,
        keys: &[Vec<u8>],
        kind: BlockKind,
        deadline_ms: u64,
        argv: Argv,
        proto: RespVersion,
    ) {
        for key in keys {
            let bc = BlockedClient {
                conn_id,
                kind,
                deadline_ms,
                argv: argv.clone(),
                proto,
            };
            self.by_key.entry(key.clone()).or_default().push_back(bc);
        }
        self.by_conn.insert(conn_id, keys.to_vec());
    }

    /// Pop and return the oldest waiter on `key`. Caller must then call
    /// `drop_for_conn(waiter.conn_id)` to scrub copies on this conn's other
    /// watched keys (multi-key BLPOP), then retry `waiter.argv`.
    pub(crate) fn pop_oldest_on_key(&mut self, key: &[u8]) -> Option<BlockedClient> {
        let queue = self.by_key.get_mut(key)?;
        let waiter = queue.pop_front();
        if queue.is_empty() {
            self.by_key.remove(key);
        }
        waiter
    }

    /// Drop every waiter copy belonging to `conn_id`. Called on (a) successful
    /// wake (purge stale copies on other keys), and (b) connection close.
    pub(crate) fn drop_for_conn(&mut self, conn_id: u64) {
        let Some(keys) = self.by_conn.remove(&conn_id) else {
            return;
        };
        for key in keys {
            let Some(queue) = self.by_key.get_mut(&key) else {
                continue;
            };
            queue.retain(|w| w.conn_id != conn_id);
            if queue.is_empty() {
                self.by_key.remove(&key);
            }
        }
    }

    /// Pop one representative waiter per conn whose `deadline_ms <= now_ms`.
    /// All copies on the conn's other watched keys are removed too, so each
    /// expired conn fires exactly one timeout reply.
    pub(crate) fn pop_expired(&mut self, now_ms: u64) -> Vec<BlockedClient> {
        let conns = self.expired_conn_ids(now_ms);
        let mut out = Vec::with_capacity(conns.len());
        for conn_id in conns {
            if let Some(rep) = self.representative(conn_id) {
                out.push(rep);
            }
            self.drop_for_conn(conn_id);
        }
        out
    }

    fn expired_conn_ids(&self, now_ms: u64) -> Vec<u64> {
        let mut seen: Vec<u64> = Vec::new();
        for queue in self.by_key.values() {
            for w in queue {
                if w.deadline_ms <= now_ms && !seen.contains(&w.conn_id) {
                    seen.push(w.conn_id);
                }
            }
        }
        seen
    }

    fn representative(&self, conn_id: u64) -> Option<BlockedClient> {
        let keys = self.by_conn.get(&conn_id)?;
        let first_key = keys.first()?;
        let queue = self.by_key.get(first_key)?;
        queue
            .iter()
            .find(|w| w.conn_id == conn_id)
            .map(|w| BlockedClient {
                conn_id: w.conn_id,
                kind: w.kind,
                deadline_ms: w.deadline_ms,
                argv: w.argv.clone(),
                proto: w.proto,
            })
    }
}

impl<C: Commands> Shard<C> {
    /// Periodic reactor tick: fire one timeout reply per blocked waiter whose
    /// `deadline_ms <= now`. Cheap when no one is parked (`is_empty()` short-
    /// circuit). Called from both the epoll and io_uring reactor loops on the
    /// same cadence as the active-TTL reaper.
    pub(crate) fn tick_blocked_timeouts(&mut self) {
        if self.blocked.is_empty() {
            return;
        }
        let now_ms = unix_now_ms();
        for w in self.blocked.pop_expired(now_ms) {
            let Some(conn) = self.conns.get_mut(&w.conn_id) else {
                continue;
            };
            conn.blocked = false;
            encode_block_timeout(&mut conn.output, w.kind, w.proto);
            self.dirty.push(w.conn_id);
        }
    }
}
