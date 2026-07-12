//! The small value types the messages carry: what a gather fetches, what it
//! got back, which multi-key reduction it is, which set/zset algebra
//! combination, how a keyspace-collection reply is shaped, and the per-write
//! metadata the dispatch path hands to its housekeeping.
//!
//! Split out of `message.rs` to keep it under the 500-LOC house cap. These are
//! parameters of the messages, not messages themselves — `Op` and `Part` stay
//! next to the runtime that folds them.

/// What to fetch per key in a cross-shard gather.
#[derive(Clone, Copy)]
pub(crate) enum GatherKind {
    /// String value (for MGET).
    Str,
    /// Set members (for SINTER/SUNION/SDIFF).
    Set,
    /// Scored members: zsets as-is, plain sets at score 1.0 (for the
    /// zset algebra family — Redis lets sets participate).
    Scored,
}

/// A single key's gathered payload.
pub(crate) enum Gathered {
    Str(Option<Vec<u8>>),
    Members(Vec<Vec<u8>>),
    /// `(member, score)` payload for [`GatherKind::Scored`].
    Scored(Vec<(Vec<u8>, f64)>),
    WrongType,
}

/// The multi-key gather reductions computed on the originating shard.
/// Public: [`crate::Route::Gather`] carries it, and embedders' `route()`
/// implementations construct it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MultiOp {
    /// `MGET` — values gathered in request order.
    Mget,
    /// `SINTER`.
    SInter,
    /// `SUNION`.
    SUnion,
    /// `SDIFF`.
    SDiff,
    /// `ZINTERCARD numkeys key… [LIMIT n]` — read-only gathered count.
    /// The `LIMIT` cap is parsed from the argv by the gather builder
    /// (it sits after the keys), not carried here.
    ZInterCard,
}

/// Which algebra combination a `*STORE` orchestrator runs after its
/// gather completes. Public: [`crate::Route::ZAlgebraStore`]
/// carries it, and embedders' `route()` implementations construct it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZCombine {
    /// `ZINTERSTORE`.
    ZInter,
    /// `ZUNIONSTORE`.
    ZUnion,
    /// `ZDIFFSTORE`.
    ZDiff,
    /// `SINTERSTORE`.
    SInter,
    /// `SUNIONSTORE`.
    SUnion,
    /// `SDIFFSTORE`.
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



