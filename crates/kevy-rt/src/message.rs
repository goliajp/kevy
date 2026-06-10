//! Internal cross-core message and aggregation types.
//!
//! These describe the work shipped between shards ([`Op`], [`Part`],
//! [`Inbound`]) and how a command's (possibly multi-shard) result is
//! accumulated on its origin shard ([`Agg`], [`PendingSlot`]). All crate-private.

use crate::BlockKind;
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

/// Write-side facts the origin's `resolve()` already computed, carried
/// with a dispatched command so the executing shard never re-parses the
/// verb. Before this rode along, every forwarded write re-ran THREE
/// full verb matches (`is_write` + `route` for the WATCH bump +
/// `wake_idx`) on the owning shard — measurable at -c50 (SET trailed
/// GET by the cost of those walks).
#[derive(Clone, Copy)]
pub(crate) struct DispatchMeta {
    pub(crate) is_write: bool,
    /// `Some(i)` = waking writes (LPUSH/RPUSH/XADD): argv[i] is the key
    /// whose blocked waiters should be woken after the write.
    pub(crate) wake_idx: Option<u8>,
    /// `Some(i)` = argv[i] is the routed key (Route::Single) — the WATCH
    /// version bump target. `None` for keyless `Route::Local` cmds.
    pub(crate) key_idx: Option<u8>,
}

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

/// A partial result shipped back to the originating shard.
pub(crate) enum Part {
    Reply(SmallReply),
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
    /// Cross-shard RENAME step 1 success: src removed; here's the
    /// value + TTL for the orchestrator to ship into step 2.
    RenameTaken {
        value: kevy_store::Value,
        ttl_ms: Option<u64>,
    },
    /// Cross-shard RENAME step 1 miss: src didn't exist.
    RenameNoSuchSrc,
    /// Cross-shard RENAME step 2 result. `refused` is `None` when the put
    /// landed at dst; `Some((value, ttl))` when `RENAMENX` blocked because
    /// dst already had an entry — the source value (taken in step 1) is
    /// handed back so the orchestrator can put it back on its shard (no
    /// data loss) before replying `:0`.
    RenamePutDone {
        refused: Option<(kevy_store::Value, Option<u64>)>,
    },
    /// `SLOWLOG GET` partial: this shard's ring buffer contents (in
    /// FIFO order — oldest first). Origin sorts by timestamp DESC and
    /// truncates per the `Get(count)` request.
    SlowlogEntries(Vec<crate::exec_slowlog::SlowlogEntry>),
    /// One stream's result for a cross-shard `XREAD` gather (see
    /// [`Op::XReadOne`]). `element` is the encoded `*2 <key> <entries>`
    /// reply element (the `*1\r\n` wrapper already stripped) when the
    /// stream had data, or `None` when empty. `index` preserves request
    /// order; an error reply is carried verbatim in `element` and detected
    /// by the leading `-`.
    XReadElement { index: u32, element: Option<Vec<u8>> },
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
    /// origin → target: drop every waiter for `(origin, conn)` — sent on
    /// successful serve, timeout, or disconnect.
    BlockCancel { origin: usize, conn: u64 },
}

/// Accumulator for a command's (possibly multi-shard) result.
pub(crate) enum Agg {
    First(Option<SmallReply>),
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
    /// Cross-shard non-blocking `XREAD` gather: each watched stream's
    /// owning shard returns its [`Part::XReadElement`], dropped into
    /// `slots` by request index. Materialized in request order, empty
    /// streams skipped (`*-1` if all empty), matching single-shard XREAD.
    XReadGather {
        slots: Vec<Option<Vec<u8>>>,
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
    /// `SLOWLOG GET` accumulator. Each shard pushes its `Vec<SlowlogEntry>`
    /// via [`Part::SlowlogEntries`]; once all replies land, materialize
    /// sorts by timestamp DESC and truncates to `count`. `count = None`
    /// means "default 10 (Redis default)"; `count = Some(n)` where `n < 0`
    /// means "all entries".
    SlowlogGet {
        count: Option<i64>,
        entries: Vec<crate::exec_slowlog::SlowlogEntry>,
    },
    /// Cross-shard RENAME / RENAMENX orchestrator. Two-step protocol:
    /// step 1 emits `Op::RenameTake` to src_shard → fold receives
    /// `Part::RenameTaken` (or `RenameNoSuchSrc`); step 2 emits
    /// `Op::RenamePut` to dst_shard → fold receives `Part::RenamePutDone`.
    /// On step transitions, `finalize_watch_agg`'s sibling
    /// `finalize_rename_agg` re-arms `slot.remaining = 1` and ships
    /// the next Op.
    RenameOrchestrator {
        /// Which step we're in (Take then Put). The taken value lives
        /// in `taken` once step 1 lands.
        step: RenameStep,
        /// `true` for `RENAMENX` — modifies step 2's reply shape (`:1`
        /// vs `+OK`) + would gate dst-overwrite (but the pre-check is
        /// in the Put-side response since cross-shard race is
        /// unavoidable without 2-phase commit; see comment in
        /// `exec_rename::finalize_rename_agg`).
        nx: bool,
        src: Vec<u8>,
        dst: Vec<u8>,
        dst_shard: usize,
        /// Value+TTL captured from step 1; populated when step
        /// transitions to Put.
        taken: Option<(kevy_store::Value, Option<u64>)>,
        /// Step 2's result, populated by fold when
        /// `Part::RenamePutDone` lands. `Some(true)` = stored,
        /// `Some(false)` = NX-blocked, `None` = step 2 hasn't run yet
        /// (we're still in Take phase).
        put_stored: Option<bool>,
    },
}

/// Phase of the cross-shard RENAME orchestrator. See [`Agg::RenameOrchestrator`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RenameStep {
    Take,
    Put,
    /// `RENAMENX` only: the Put was NX-refused (dst already existed), so
    /// the source taken in step 1 is being put back on its shard before
    /// the `:0` reply — a no-op `RENAMENX` must not lose the source.
    Restore,
}

/// One outstanding command slot awaiting `remaining` sub-results, held in a
/// per-connection seq-ordered ring.
pub(crate) struct PendingSlot {
    pub(crate) remaining: u32,
    pub(crate) agg: Agg,
    /// Materialized reply once `remaining == 0`; emitted in seq order.
    /// `SmallReply` so the forwarded tiny-reply path (+OK / :N / small
    /// GET) stays heap-free end to end.
    pub(crate) done: Option<SmallReply>,
    /// RESP version captured at dispatch time. Cross-shard gathers
    /// (SINTER / SUNION / SDIFF) materialise on the origin shard long
    /// after `start_multi` snapped this conn's proto; storing it here
    /// (vs. re-reading `conn.proto` at fold time) keeps each in-flight
    /// cmd shaped per the proto it was dispatched under — a HELLO 3
    /// after `start_multi` doesn't retroactively reshape its reply.
    /// 1 byte + alignment padding; not on any hot path.
    pub(crate) proto: RespVersion,
}
