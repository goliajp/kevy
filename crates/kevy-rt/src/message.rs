//! Internal cross-core message and aggregation types.
//!
//! These describe the work shipped between shards ([`Op`], [`Part`],
//! [`Inbound`]) and how a command's (possibly multi-shard) result is
//! accumulated on its origin shard ([`Agg`], [`PendingSlot`]). All crate-private.

use crate::BlockKind;
use kevy_resp::{Argv, RespVersion};

pub use crate::message_kinds::{MultiOp, ZCombine};
pub(crate) use crate::message_part::Part;
pub(crate) use crate::message_kinds::{DispatchMeta, GatherKind, Gathered};
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

/// A unit of work shipped to the owning shard. Forwarded single-key
/// commands don't ride here — they go through the batched
/// [`Inbound::RequestBatch`] lane (one `(conn, seq, Argv, RespVersion,
/// DispatchMeta)` entry each) and execute via `Shard::run_dispatch`.
pub(crate) enum Op {
    Del(Vec<Vec<u8>>),
    Exists(Vec<Vec<u8>>),
    Dbsize,
    Flush,
    Save,
    /// Background snapshot: freeze a COW view now, persist off-thread.
    BgSave,
    /// Rebuild the AOF from this shard's in-memory state (BGREWRITEAOF),
    /// serialized off-thread from a COW view.
    RewriteAof,
    /// Set these key/value pairs (MSET).
    MSet(KvPairs),
    /// Fetch per-key payloads (MGET / set algebra).
    Gather(GatherKind, Vec<Vec<u8>>),
    /// Step-2 of the zset-algebra orchestrator: materialize the
    /// combined result at `dst` on its owning shard (overwrite; empty
    /// deletes). Replies `Part::Int(cardinality)`.
    ZStoreResult { dst: Vec<u8>, pairs: Vec<(Vec<u8>, f64)> },
    /// Set-form step-2 (`SINTERSTORE` family).
    SetStoreResult { dst: Vec<u8>, members: Vec<Vec<u8>> },
    /// FEED.READ executed on the target shard.
    FeedRead {
        cursor_gen: u64,
        offset: u64,
        count: usize,
        prefixes: Vec<Vec<u8>>,
    },
    /// FEED.TAIL executed on the target shard.
    FeedTail,
    /// Extension fan-out: run `Commands::extension_op` on this
    /// shard with the original argv; reply is an opaque chunk.
    /// Shared, not cloned: the same argv goes to every shard, and copying
    /// ten byte-strings sixteen times to hand each thread its own set was
    /// ~160 allocations a query for bytes nobody mutates.
    Extension { argv: std::sync::Arc<[Vec<u8>]> },
    /// Step-1 of the geo `*STORE` orchestrator: run the search half of
    /// `GEOSEARCHSTORE` / `GEORADIUS[BYMEMBER] … STORE` on the SOURCE key's
    /// shard (read-only — the destination write is a separate
    /// [`Op::ZStoreResult`] on the destination's shard). Reply
    /// [`Part::GeoHits`].
    GeoSearch { argv: Vec<Vec<u8>> },
    /// `REPL.TOKEN` fan-out: read this shard's live
    /// `(feed generation, next_offset)` pair. Reply [`Part::ReplToken`].
    /// Live (not tick-stale): a token minted right after a write must
    /// cover that write.
    ReplToken,
    /// `PREFIX.STATS <prefix>` — per-shard prefix walk, summed at
    /// the origin.
    PrefixStats(Vec<u8>),
    /// `CLIENT LIST` — render this shard's conn-table rows; reply is
    /// an opaque text chunk ([`Part::ExtensionChunk`]).
    ClientList,
    /// `CLIENT KILL` — close this shard's conns matching the selector;
    /// reply is the matched count ([`Part::Int`]).
    ClientKill(crate::client_ops::ClientKillFilter),
    /// Collect this shard's matching keys — KEYS. (SCAN pages through
    /// [`Op::ScanStep`]; RANDOMKEY draws through [`Op::RandomKey`].)
    CollectKeys(Option<Vec<u8>>, Option<usize>),
    /// One arbitrary key from this shard, plus the weight and randomness the
    /// origin needs to fold candidates fairly (see [`Part::RandomKey`]).
    RandomKey,
    /// One `SCAN` page on this shard: walk ~`count` buckets from the
    /// in-shard `cursor` (reverse-binary, rehash-tolerant — see
    /// [`kevy_store::Store::scan_page`]), applying the MATCH glob and
    /// TYPE filter. Reply: [`Part::ScanPage`].
    ScanStep {
        cursor: u64,
        count: usize,
        pattern: Option<Vec<u8>>,
        type_filter: Option<Vec<u8>>,
    },
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
    /// `RENAME` / `RENAMENX` — both keys on the same shard. Atomic on
    /// that shard via [`kevy_store::Store::rename`]. Reply: `Part::Reply`
    /// carrying `+OK\r\n` (RENAME ok), `:1\r\n` / `:0\r\n` (RENAMENX
    /// ok / dst-exists), or `-ERR no such key\r\n`.
    Rename {
        src: Vec<u8>,
        dst: Vec<u8>,
        /// `true` for `RENAMENX` semantics (no overwrite — reply `:0`
        /// if dst exists; reply `:1` on successful rename).
        nx: bool,
    },
    /// Cross-shard RENAME step 1: atomically take `src` (entry + TTL)
    /// off this shard. Reply `Part::RenameTaken` on success or
    /// `Part::RenameNoSuchSrc` if the key doesn't exist. The
    /// orchestrator on the origin shard chains the value into a
    /// follow-up [`Op::RenamePut`] on the destination shard.
    RenameTake(Vec<u8>),
    /// Cross-shard RENAME step 2: store the just-taken value at `dst`
    /// on this shard. If `nx` is set and dst already exists, the put
    /// is refused — orchestrator must rollback (restore src) or accept
    /// loss. Reply: `Part::RenamePutDone { stored: bool }`.
    RenamePut {
        dst: Vec<u8>,
        value: kevy_store::Value,
        ttl_ms: Option<u64>,
        nx: bool,
    },
    /// Cross-shard BITOP step 2: store the combined bytes at `key`, or
    /// delete `key` when they are empty. Reply [`Part::Int`] with the
    /// stored length.
    BitOpResult { key: Vec<u8>, value: Vec<u8> },
    /// Same-shard COPY: both keys hash here, so one atomic
    /// clone-then-put. Reply [`Part::CopyPutDone`].
    Copy { src: Vec<u8>, dst: Vec<u8>, replace: bool },
    /// Cross-shard COPY step 1: clone `src`'s value and its remaining
    /// TTL WITHOUT removing it. That one word is the whole difference
    /// from [`Op::RenameTake`], and it is why this family needs no
    /// Restore step: a refused put leaves the source where it was.
    /// Reply [`Part::CopyRead`], `None` when `src` does not exist.
    CopyRead(Vec<u8>),
    /// Cross-shard COPY step 2: place the clone at `dst` on this shard.
    /// Refused, without a rollback to arrange, when `dst` exists and
    /// `replace` is not set. Reply [`Part::CopyPutDone`].
    CopyPut {
        dst: Vec<u8>,
        value: kevy_store::Value,
        ttl_ms: Option<u64>,
        replace: bool,
    },
    /// Same-shard list move — one atomic pop+push on the owning shard.
    /// Reply [`Part::ListMoved`].
    ListMove {
        src: Vec<u8>,
        dst: Vec<u8>,
        from_left: bool,
        to_left: bool,
    },
    /// Cross-shard list move step 1: pop one element off `key` on this
    /// shard. Reply [`Part::ListMoveTaken`] — `None` when the source is
    /// empty or absent, which the orchestrator turns into a nil reply
    /// without ever touching the destination.
    ListMoveTake { key: Vec<u8>, from_left: bool },
    /// Cross-shard list move step 2: push the taken element onto `key` on
    /// this shard. Reply [`Part::ListMovePushed`] — `refused` carries the
    /// element back when the destination exists and is not a list, so the
    /// orchestrator can put it back where it came from instead of dropping
    /// it on the floor.
    ListMovePush {
        key: Vec<u8>,
        value: Vec<u8>,
        to_left: bool,
    },
    /// Cross-shard list move rollback: the destination refused the element
    /// (WRONGTYPE), so put it back on the source, at the end it came from.
    /// Reply [`Part::Ok`] — the orchestrator has already decided the client
    /// gets `-WRONGTYPE`.
    ListMoveRestore {
        key: Vec<u8>,
        value: Vec<u8>,
        from_left: bool,
    },
    /// `SLOWLOG GET` — collect this shard's ring buffer. Reply
    /// [`Part::SlowlogEntries`] with a clone of the deque (origin
    /// sorts + truncates after merging across shards).
    SlowlogGet,
    /// `SLOWLOG LEN` — this shard's ring length. Reply [`Part::Int`].
    SlowlogLen,
    /// `SLOWLOG RESET` — clear this shard's ring. Reply [`Part::Ok`].
    SlowlogReset,
    /// One stream of a multi-stream non-blocking `XREAD` / `XREADGROUP`
    /// whose streams span shards. `argv` is a complete single-stream
    /// rewrite (`XREAD [COUNT n] STREAMS key id` or `XREADGROUP GROUP g c
    /// [COUNT n] [NOACK] STREAMS key id`) dispatched on the stream's owning
    /// shard (so `$` resolves to that shard's `last_id`); `index` is the
    /// stream's position in the original request, used to reassemble the
    /// reply in request order. `write` marks the XREADGROUP form — it
    /// mutates group state (PEL / last-delivered), so the owning shard runs
    /// the post-write housekeeping (AOF log of the rewritten argv, WATCH
    /// bump, keyspace notify) after dispatch. Reply: [`Part::XReadElement`].
    XReadOne { index: u32, argv: Argv, write: bool },
}



/// A RESP reply fragment with a 30-byte inline arm. The forwarded-dispatch
/// hot path produces tiny replies (`+OK`, `:N`, a `$16` GET payload = 23 B)
/// whose heap `Vec` round-trip (alloc on the owning shard, free after the
/// origin's drain) dominated the data itself — ~19 % of 8-shard SET CPU sat
/// in the allocator. `Inline` keeps those entirely on the stack across the
/// ring; `Heap` carries anything bigger with the old one-alloc semantics.
pub(crate) enum SmallReply {
    Inline { len: u8, buf: [u8; 30] },
    Heap(Vec<u8>),
}

impl SmallReply {
    /// Copy `b` into the inline arm when it fits, else one heap alloc.
    #[inline]
    pub(crate) fn from_slice(b: &[u8]) -> Self {
        if b.len() <= 30 {
            let mut buf = [0u8; 30];
            buf[..b.len()].copy_from_slice(b);
            SmallReply::Inline { len: b.len() as u8, buf }
        } else {
            SmallReply::Heap(b.to_vec())
        }
    }

    /// Wrap an already-owned `Vec` — zero-copy for the heap arm.
    #[inline]
    pub(crate) fn from_vec(v: Vec<u8>) -> Self {
        SmallReply::Heap(v)
    }

    #[inline]
    pub(crate) fn as_slice(&self) -> &[u8] {
        match self {
            SmallReply::Inline { len, buf } => &buf[..*len as usize],
            SmallReply::Heap(v) => v,
        }
    }
}


/// A batch of single-key dispatches forwarded to one owning shard:
/// `(conn, seq, argv, proto)` each. Batched per loop so a -c50 flood
/// costs one cross-core send per target shard, not one per command.
/// The per-entry `proto` lets a single batch carry cmds from V2 and V3
/// conns to the same owning shard.
pub(crate) type ReqBatch = Vec<(u64, u64, Argv, RespVersion, DispatchMeta)>;
/// The matching replies `(conn, seq, part)` sent back as one message.
/// Each reply carries the request's spent `Argv` husk back to the origin,
/// which drops it into its own [`kevy_resp::ArgvPool`] — so every shard's
/// pool level matches its own conn demand by construction, immune to
/// accept skew (a conn-heavy shard forwards more than it receives, so
/// recycle-at-the-owner starves its pool while overfilling quiet shards').
pub(crate) type RespBatch = Vec<(u64, u64, Part, Argv)>;

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

    // ── Cross-shard BLOCK arbiter (see [`crate::block_xshard`]) ──
    // A conn parks on its origin shard; watch registrations fan out to the
    // shards owning each watched key. The origin is the single arbiter that
    // decides which ready key serves the conn, so no target ever pops
    // speculatively (which would lose data when two keys go ready at once).
    /// origin → target: "watch `key` for `(origin, conn)`; if a replay of
    /// `serve_argv` would yield data now, send back [`Inbound::BlockReady`]".
    /// Re-sent verbatim to re-arm after a raced-empty serve (idempotent —
    /// the target dedups by `(origin, conn, key)`).
    BlockArm {
        origin: usize,
        conn: u64,
        key: Vec<u8>,
        kind: BlockKind,
        serve_argv: Argv,
        /// The origin conn's RESP version, so the target shapes the served
        /// reply (V2 array / V3 map) correctly without a round-trip.
        proto: RespVersion,
    },
    /// target → origin: a watched `key` may now satisfy `conn`. The origin
    /// arbitrates (ignores if `conn` already served / serving).
    BlockReady { conn: u64, key: Vec<u8> },
    /// origin → target: "serve `key` for `(origin, conn)` now" — the target
    /// replays the armed `serve_argv` (popping / consuming) and returns the
    /// reply via [`Inbound::BlockServeResp`].
    BlockServeReq {
        origin: usize,
        conn: u64,
        key: Vec<u8>,
    },
    /// target → origin: the serve result. Empty `reply` = raced (another
    /// client drained the key between ready and serve) → the origin re-arms.
    BlockServeResp {
        conn: u64,
        key: Vec<u8>,
        reply: Vec<u8>,
    },
    /// origin → target: the serve landed on a live client — release the
    /// undo the target is holding for `(origin, conn)`.
    BlockServeAck { origin: usize, conn: u64 },
    /// origin → src's shard: a cross-shard RENAME's put committed on the
    /// destination, so the source may now record its half (the delete).
    /// Sent only after the put succeeded — see
    /// `Shard::log_rename_source_committed` for why not before.
    RenameCommitted { src: Vec<u8> },
    /// origin → target: the serve could NOT be delivered (the client
    /// disconnected while it was in flight) — apply the held undo, so
    /// the popped element goes back instead of vanishing.
    BlockServeAbort { origin: usize, conn: u64 },
    /// origin → target: drop every waiter for `(origin, conn)` — sent on
    /// successful serve, timeout, or disconnect.
    BlockCancel { origin: usize, conn: u64 },

    // ── Replication waiters (see [`crate::exec_replwait`]) ──
    // WAIT / REPL.WAIT arm-and-defer messages ride their own Inbound
    // lane rather than `Op`/`Response`: the reply may come seconds
    // later (ACK arrival / apply progress / deadline), and it must NOT
    // participate in `xshard_inflight` accounting — a parked waiter
    // would otherwise pin the origin core in the busy-poll rung for
    // the whole wait.
    /// origin → target: `WAIT` — answer with the number of
    /// replicas that acked this shard's `master_repl_offset` (frozen
    /// at arm receipt), as soon as that count reaches `need` or
    /// `deadline_ms` passes. Reply: [`Inbound::ReplDone`].
    ReplWaitArm {
        origin: usize,
        conn: u64,
        seq: u64,
        need: u32,
        deadline_ms: u64,
    },
    /// origin → target: `REPL.WAIT` — answer 1 once this
    /// shard's replication-apply position reaches `min_offset`, or 0
    /// when `deadline_ms` passes. Reply: [`Inbound::ReplDone`].
    ReplApplyArm {
        origin: usize,
        conn: u64,
        seq: u64,
        min_offset: u64,
        deadline_ms: u64,
    },
    /// target → origin: one shard's WAIT / REPL.WAIT answer, folded as
    /// `Part::Int(n)` into the pending slot ([`Agg::MinInt`] /
    /// [`Agg::ReplBarrier`]).
    ReplDone { conn: u64, seq: u64, n: i64 },
}


// The aggregation half (`Agg` / `RenameStep` / `PendingSlot`) lives in
// [`crate::message_agg`] — split out so this file stays under the
// 500-LOC house rule. Re-exported here so every user keeps its
// `crate::message::…` path.
pub(crate) use crate::message_agg::{Agg, PendingSlot, RenameStep};
