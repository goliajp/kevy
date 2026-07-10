//! The origin-shard aggregation half of [`crate::message`]: how a
//! command's (possibly multi-shard) result is accumulated ([`Agg`]) and
//! held awaiting sub-results ([`PendingSlot`]). Split out of `message.rs`
//! (500-LOC house rule); `message` re-exports everything, so paths are
//! unchanged. All crate-private.

use crate::message::{Gathered, KeyShape, MultiOp, SmallReply, ZCombine};
use kevy_resp::{Argv, RespVersion};
use std::collections::HashMap;

/// Accumulator for a command's (possibly multi-shard) result.
pub(crate) enum Agg {
    First(Option<SmallReply>),
    SumInt(i64),
    /// v3.16 D1 `WAIT` accumulator: MIN over the per-shard acked-replica
    /// counts (starts at `i64::MAX`; every shard folds one `Part::Int`).
    MinInt(i64),
    /// v3.16 D2 `REPL.WAIT` accumulator: every shard folds `Part::Int`
    /// (1 = applied barrier met, 0 = deadline passed). All 1 → `+OK`;
    /// any 0 → the pre-built `miss` reply bytes.
    ReplBarrier { ok: bool, miss: Vec<u8> },
    /// v3.16 D2 `REPL.TOKEN` accumulator: per-shard `(generation,
    /// next_offset)` pairs dropped in by shard id, materialized as one
    /// flat `[gen0, off0, gen1, off1, …]` integer array.
    ReplTokens { slots: Vec<Option<(u64, u64)>> },
    AllOk,
    /// Gathered per-key payloads, reduced by `op` over `keys` (request order).
    Gather {
        op: MultiOp,
        keys: Vec<Vec<u8>>,
        got: HashMap<Vec<u8>, Gathered>,
    },
    /// v2.3 PREFIX.STATS accumulator (summed across shards).
    PrefixStats { keys: u64, expires: u64 },
    /// v2.5 extension fan-out accumulator; reduced by
    /// `Commands::extension_reduce` when the last chunk lands.
    ExtensionGather { argv: Vec<Vec<u8>>, chunks: Vec<Vec<u8>> },
    /// v2.2 zset-algebra `*STORE` orchestrator, step 1: gather scored
    /// (or set) members per source key; on completion the origin
    /// computes the combination and ships `Op::ZStoreResult` /
    /// `Op::SetStoreResult` to `dst`'s shard (step 2 folds through a
    /// re-armed `Agg::SumInt`).
    ZStoreGather {
        combine: ZCombine,
        weights: Option<Vec<f64>>,
        aggregate: kevy_store::ZAggregate,
        dst: Vec<u8>,
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
