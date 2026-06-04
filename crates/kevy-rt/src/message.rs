//! Internal cross-core message and aggregation types.
//!
//! These describe the work shipped between shards ([`Op`], [`Part`],
//! [`Inbound`]) and how a command's (possibly multi-shard) result is
//! accumulated on its origin shard ([`Agg`], [`PendingSlot`]). All crate-private.

use kevy_resp::{Argv, RespVersion};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// A list of key/value pairs (for MSET).
pub(crate) type KvPairs = Vec<(Vec<u8>, Vec<u8>)>;

/// Shared pub/sub channel registry: `channel → (global subscriber count, bitset
/// of shard ids that have ≥1 subscriber)`. Written on SUBSCRIBE/UNSUBSCRIBE/conn
/// close (rare); read on every PUBLISH (hot) so the publisher can reply with the
/// receiver count **locally** (no cross-shard count aggregation) and fan the
/// delivery out **only** to shards that hold a subscriber. The bitset is an
/// over-approximation between a channel's first sub and its count reaching 0
/// (cleared then) — safe, since a stray delivery just finds no local subscriber.
pub(crate) type PubSubReg = Arc<RwLock<HashMap<Vec<u8>, (u32, u64)>>>;

/// Shared pub/sub pattern registry: `pattern → (global subscriber count,
/// bitset of shard ids that have ≥1 subscriber to this pattern)`. Like
/// [`PubSubReg`] but for `PSUBSCRIBE` patterns. PUBLISH walks this Vec
/// linearly running [`kevy_store::glob_match`] against each pattern;
/// matchers contribute to the reply count and the union shard bitset that
/// receives the publish delivery. A `Vec<(...)>` (not a HashMap) because
/// the keyspace is patterns, not exact strings — we have to glob_match
/// every entry no matter how it's stored. The pmessage fan-out plus the
/// channel-precise path remain disjoint code paths so the channel-only
/// PUBLISH hot path is undisturbed by the existence of pattern subscribers.
pub(crate) type PubSubPatternReg = Arc<RwLock<Vec<(Vec<u8>, u32, u64)>>>;

/// One pub/sub message `(channel, payload)`, shared (not cloned) across the
/// shards it fans out to.
pub(crate) type PubMsg = Arc<(Vec<u8>, Vec<u8>)>;

/// What to fetch per key in a cross-shard gather.
#[derive(Clone, Copy)]
pub(crate) enum GatherKind {
    /// String value (for MGET).
    Str,
    /// Set members (for SINTER/SUNION/SDIFF).
    Set,
}

/// A single key's gathered payload.
pub(crate) enum Gathered {
    Str(Option<Vec<u8>>),
    Members(Vec<Vec<u8>>),
    WrongType,
}

/// The multi-key reductions computed on the originating shard.
#[derive(Clone, Copy)]
pub(crate) enum MultiOp {
    Mget,
    SInter,
    SUnion,
    SDiff,
}

/// A unit of work shipped to the owning shard.
pub(crate) enum Op {
    /// Forward a single command. The trailing `RespVersion` rides with
    /// each cmd (not the conn) so a V2 client and a V3 client can hand
    /// requests to the same owning shard and each get the right reply
    /// shape from `dispatch` vs `dispatch_resp3`. Defaults to V2 for
    /// any backwards-compatible construction site.
    Dispatch(Argv, RespVersion),
    Del(Vec<Vec<u8>>),
    Exists(Vec<Vec<u8>>),
    Dbsize,
    Flush,
    Save,
    /// Rebuild the AOF from this shard's in-memory state (BGREWRITEAOF).
    RewriteAof,
    /// Set these key/value pairs (MSET).
    MSet(KvPairs),
    /// Fetch per-key payloads (MGET / set algebra).
    Gather(GatherKind, Vec<Vec<u8>>),
    /// Collect this shard's keys (optional glob + limit) — KEYS/SCAN/RANDOMKEY.
    CollectKeys(Option<Vec<u8>>, Option<usize>),
    /// `WATCH key [key ...]` — register each key in this shard's
    /// version tracker and report its current version back. The origin
    /// shard collates the (key, version) pairs into the conn's
    /// `watched` set; `EXEC` later asks every owning shard whether
    /// the version is still current via [`Op::CheckWatch`].
    CollectWatchVersions(Vec<Vec<u8>>),
    /// `EXEC`'s pre-execution fan-out: for each `(key, version)` pair,
    /// compare against this shard's current `key_version(key)`. The
    /// reply ([`Part::Int`]) is `1` if ANY key on this shard has been
    /// modified since the recorded version, else `0`. The origin shard
    /// ORs the partial replies and aborts EXEC on any `1`.
    CheckWatch(Vec<(Vec<u8>, u64)>),
}

/// How a KEYS-family reply is shaped.
#[derive(Clone, Copy)]
pub(crate) enum KeyShape {
    /// `KEYS` — a flat array of keys.
    Keys,
    /// `SCAN` — `[cursor, [keys]]` (cursor always "0").
    Scan,
    /// `RANDOMKEY` — one key as a bulk string, or nil.
    Random,
}

/// A partial result shipped back to the originating shard.
pub(crate) enum Part {
    Reply(Vec<u8>),
    Int(i64),
    Ok,
    /// Per-key gathered payloads.
    Gathered(Vec<(Vec<u8>, Gathered)>),
    /// A shard's collected keys (KEYS/SCAN/RANDOMKEY).
    Keys(Vec<Vec<u8>>),
    /// `WATCH` partial reply: each key this shard owns paired with its
    /// current version, in request order. The origin shard collates
    /// these into the conn's watched set.
    WatchVersions(Vec<(Vec<u8>, u64)>),
}

/// A batch of single-key dispatches forwarded to one owning shard:
/// `(conn, seq, argv, proto)` each. Batched per loop so a -c50 flood
/// costs one cross-core send per target shard, not one per command.
/// The per-entry `proto` lets a single batch carry cmds from V2 and V3
/// conns to the same owning shard.
pub(crate) type ReqBatch = Vec<(u64, u64, Argv, RespVersion)>;
/// The matching replies `(conn, seq, part)` sent back as one message.
pub(crate) type RespBatch = Vec<(u64, u64, Part)>;

/// Inter-core message (each core has one inbound queue carrying both).
pub(crate) enum Inbound {
    Request {
        origin: usize,
        conn: u64,
        seq: u64,
        op: Op,
    },
    Response {
        conn: u64,
        seq: u64,
        part: Part,
    },
    /// Batched single-key dispatches to this (owning) shard; replied as one
    /// `ResponseBatch`. The hot -c50 path: amortizes the cross-core ring/fold
    /// overhead that drags 16 shards below 1 (single-shard is 2.1M GET).
    RequestBatch {
        origin: usize,
        reqs: ReqBatch,
    },
    /// Batched replies for a `RequestBatch`, folded by seq on the origin.
    ResponseBatch(RespBatch),
    /// A batch of pub/sub messages `(channel, payload)` to deliver to this
    /// shard's subscribers — fire-and-forget (no reply; the publisher already
    /// replied with the receiver count from the registry). Batched per drain so
    /// a flood of PUBLISHes costs one cross-core send per target shard, not one
    /// per message. `Arc` so the same payload fanned to many shards is shared,
    /// not cloned per target.
    DeliverPublish(Vec<PubMsg>),
}

/// Accumulator for a command's (possibly multi-shard) result.
pub(crate) enum Agg {
    First(Option<Vec<u8>>),
    SumInt(i64),
    AllOk,
    /// Gathered per-key payloads, reduced by `op` over `keys` (request order).
    Gather {
        op: MultiOp,
        keys: Vec<Vec<u8>>,
        got: HashMap<Vec<u8>, Gathered>,
    },
    /// Keys collected from all shards, shaped per `KeyShape`.
    Keys {
        shape: KeyShape,
        acc: Vec<Vec<u8>>,
    },
    /// `WATCH` fan-out accumulator: each owning shard returns its
    /// `(key, version)` pairs via [`Part::WatchVersions`]; the origin
    /// shard appends them all and, when the last fan-out reply arrives,
    /// moves the pairs into the connection's `watched` set + emits +OK.
    WatchCollect {
        pairs: Vec<(Vec<u8>, u64)>,
    },
    /// `EXEC` pre-execution accumulator: a non-empty WATCH set fans
    /// `CheckWatch` out to every shard that owns a watched key. Each
    /// reply ORs into `dirty`. When the last reply arrives, the origin
    /// shard either aborts (dirty → header = `*-1\r\n`, every queued
    /// placeholder slot emits 0 bytes) or commits (clean → header =
    /// `*N\r\n`, then dispatches each `queued` cmd at its pre-allocated
    /// seq via `start_command_at_seq`).
    ExecPrep {
        dirty: bool,
        queued: Vec<Argv>,
        header_seq: u64,
    },
}

/// One outstanding command slot awaiting `remaining` sub-results, held in a
/// per-connection seq-ordered ring.
pub(crate) struct PendingSlot {
    pub(crate) remaining: u32,
    pub(crate) agg: Agg,
    /// Materialized reply once `remaining == 0`; emitted in seq order.
    pub(crate) done: Option<Vec<u8>>,
}
