// The pieces consumed by the follow-up XREAD BLOCK / XREADGROUP BLOCK
// sprints (`BlockKind::XReadBlock`, `BlockKind::XReadGroupBlock`,
// `BlockHint::XReadBlock`, …) are marked here; once those sprints connect
// callers the corresponding warnings re-fire automatically.
#![expect(
    dead_code,
    reason = "stream BlockKind / BlockHint variants land in v2-7d.3 / .4"
)]

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
        .map_or(0, |d| d.as_millis() as u64)
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
        (RespVersion::V2, BlockKind::Blpop | BlockKind::Brpop | BlockKind::Bzpopmin) => {
            out.extend_from_slice(b"*-1\r\n");
        }
        (RespVersion::V2, BlockKind::XReadBlock | BlockKind::XReadGroupBlock) => {
            out.extend_from_slice(b"$-1\r\n");
        }
        // BRPOPLPUSH on timeout returns nil bulk (the would-be moved
        // element). Same shape as XREAD timeout.
        (RespVersion::V2, BlockKind::Brpoplpush) => {
            out.extend_from_slice(b"$-1\r\n");
        }
    }
}

/// Which blocking command a waiter is parked in. Drives both timeout-nil
/// shape and wake-retry dispatch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockKind {
    Blpop,
    Brpop,
    /// `BZPOPMIN key [key ...] timeout` — block until a sorted set has a
    /// member, then pop the lowest-scored one. Same arm-and-serve flow as
    /// `BLPOP`; the reply shape adds a third bulk (the score).
    Bzpopmin,
    /// `BRPOPLPUSH source destination timeout` — atomic blocking
    /// right-pop from `source` + left-push to `destination`. Parks
    /// on `source` only. Reply: single bulk of the moved element on
    /// success, nil bulk on timeout. Deprecated since Redis 6.2 in
    /// favour of BLMOVE, but Bee Queue (and many older clients)
    /// still emit it.
    Brpoplpush,
    XReadBlock,
    XReadGroupBlock,
}

/// How a command wants to block, if at all. Returned by
/// [`Commands::resolve`] inside [`crate::ResolvedCmd`] so the verb-table
/// lookup happens once per command. `None` is the zero-cost default for
/// every non-blocking verb (≥ 99.9 % of dispatches in steady state).
///
/// `keys` is every key the conn watches (≥ 1). The dispatcher picks the
/// park strategy from them:
/// - **single key on the conn's own shard** → the in-shard fast path
///   (`BlockedClients`): register + wake without any cross-core hop.
/// - **single remote key, or any multi-key form** → the cross-shard
///   arbiter (`block_xshard`): the conn parks on its origin
///   shard and watch registrations fan out to each key's owning shard.
///
/// For `BLPOP` / `BRPOP` the keys are list keys; for `XREAD BLOCK` /
/// `XREADGROUP BLOCK` they are the STREAMS keys (in request order).
#[derive(Clone, Debug, Default)]
pub enum BlockHint {
    #[default]
    None,
    Block {
        kind: BlockKind,
        keys: Vec<Vec<u8>>,
        /// `0` = block forever (Redis convention). Anything else is the
        /// wall-clock millis the dispatcher will add to `unix_now_ms()` to
        /// derive the waiter's `deadline_ms`.
        timeout_ms: u64,
    },
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

    /// Wake the oldest waiter on `key` (FIFO, matching Redis) and retry its
    /// command. Called by the dispatcher after a write that may have produced
    /// new data for blocked readers — `LPUSH` / `RPUSH` for `BLPOP` /
    /// `BRPOP`; `XADD` for `XREAD BLOCK` / `XREADGROUP BLOCK`. The retry
    /// re-runs the original command via `Commands::dispatch_into`; if the
    /// data has already been consumed in a race window, the retry sees an
    /// empty list / stream and a `None` from this fn — the waiter has
    /// already been popped out of the registry so it stays unblocked (the
    /// next tick or a fresh client request resolves it). One push wakes one
    /// waiter only (Redis semantics — a single LPUSH does not feed two
    /// BLPOP clients).
    pub(crate) fn wake_blocked_on_key(&mut self, key: &[u8]) {
        if self.blocked.is_empty() {
            return;
        }
        let Some(waiter) = self.blocked.pop_oldest_on_key(key) else {
            return;
        };
        self.blocked.drop_for_conn(waiter.conn_id);
        let Some(conn) = self.conns.get_mut(&waiter.conn_id) else {
            return;
        };
        conn.blocked = false;
        let proto = waiter.proto;
        match proto {
            RespVersion::V2 => self
                .commands
                .dispatch_into(&mut self.store, &waiter.argv, &mut conn.output),
            RespVersion::V3 => self
                .commands
                .dispatch_into_resp3(&mut self.store, &waiter.argv, &mut conn.output),
        }
        conn.next_emit += 1;
        self.dirty.push(waiter.conn_id);
    }
}
