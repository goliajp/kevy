//! Internal cross-core message and aggregation types.
//!
//! These describe the work shipped between shards ([`Op`], [`Part`],
//! [`Inbound`]) and how a command's (possibly multi-shard) result is
//! accumulated on its origin shard ([`Agg`], [`PendingSlot`]). All crate-private.

use kevy_resp::Argv;
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
    Dispatch(Argv),
    Del(Vec<Vec<u8>>),
    Exists(Vec<Vec<u8>>),
    Dbsize,
    Flush,
    Save,
    /// Set these key/value pairs (MSET).
    MSet(KvPairs),
    /// Fetch per-key payloads (MGET / set algebra).
    Gather(GatherKind, Vec<Vec<u8>>),
    /// Collect this shard's keys (optional glob + limit) — KEYS/SCAN/RANDOMKEY.
    CollectKeys(Option<Vec<u8>>, Option<usize>),
    /// Deliver `message` to this shard's subscribers of `channel`; return count.
    Publish(Vec<u8>, Vec<u8>),
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
}

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
    /// Fire-and-forget work with no reply folded back (pub/sub message delivery:
    /// the publisher already replied with the receiver count from the registry).
    Deliver {
        op: Op,
    },
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
}

/// One outstanding command slot awaiting `remaining` sub-results, held in a
/// per-connection seq-ordered ring.
pub(crate) struct PendingSlot {
    pub(crate) remaining: u32,
    pub(crate) agg: Agg,
    /// Materialized reply once `remaining == 0`; emitted in seq order.
    pub(crate) done: Option<Vec<u8>>,
}
