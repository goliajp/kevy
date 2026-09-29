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
    /// String value, with a wrong-type key reported AS wrong-type
    /// rather than as absent (for BITOP, which Redis errors on where
    /// MGET answers nil).
    StrStrict,
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
///
/// ```
/// let route = kevy_rt::Route::Gather(kevy_rt::MultiOp::Mget);
/// assert!(matches!(route, kevy_rt::Route::Gather(kevy_rt::MultiOp::Mget)));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MultiOp {
    /// `MGET` — values gathered in request order.
    ///
    /// ```
    /// use kevy_rt::{MultiOp, Route};
    ///
    /// // What a `route()` answers for `MGET a b`: gather every key, reduce at the origin.
    /// let route = Route::Gather(MultiOp::Mget);
    /// assert!(matches!(route, Route::Gather(MultiOp::Mget)));
    /// ```
    Mget,
    /// `SINTER`.
    ///
    /// ```
    /// use kevy_rt::{MultiOp, Route};
    ///
    /// // What a `route()` answers for `SINTER a b`: gather every key, reduce at the origin.
    /// let route = Route::Gather(MultiOp::SInter);
    /// assert!(matches!(route, Route::Gather(MultiOp::SInter)));
    /// ```
    SInter,
    /// `SUNION`.
    ///
    /// ```
    /// use kevy_rt::{MultiOp, Route};
    ///
    /// // What a `route()` answers for `SUNION a b`: gather every key, reduce at the origin.
    /// let route = Route::Gather(MultiOp::SUnion);
    /// assert!(matches!(route, Route::Gather(MultiOp::SUnion)));
    /// ```
    SUnion,
    /// `SDIFF`.
    ///
    /// ```
    /// use kevy_rt::{MultiOp, Route};
    ///
    /// // What a `route()` answers for `SDIFF a b`: gather every key, reduce at the origin.
    /// let route = Route::Gather(MultiOp::SDiff);
    /// assert!(matches!(route, Route::Gather(MultiOp::SDiff)));
    /// ```
    SDiff,
    /// `ZINTERCARD numkeys key… [LIMIT n]` — read-only gathered count.
    /// The `LIMIT` cap is parsed from the argv by the gather builder
    /// (it sits after the keys), not carried here.
    ///
    /// ```
    /// use kevy_rt::{MultiOp, Route};
    ///
    /// // What a `route()` answers for `ZINTERCARD 2 a b`: gather every key, reduce at the origin.
    /// let route = Route::Gather(MultiOp::ZInterCard);
    /// assert!(matches!(route, Route::Gather(MultiOp::ZInterCard)));
    /// ```
    ZInterCard,
}

/// Which algebra combination a `*STORE` orchestrator runs after its
/// gather completes. Public: [`crate::Route::ZAlgebraStore`]
/// carries it, and embedders' `route()` implementations construct it.
///
/// ```
/// let route = kevy_rt::Route::ZAlgebraStore(kevy_rt::ZCombine::ZUnion);
/// assert_ne!(route, kevy_rt::Route::ZAlgebraStore(kevy_rt::ZCombine::ZInter));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ZCombine {
    /// `ZINTERSTORE`.
    ///
    /// ```
    /// use kevy_rt::{Route, ZCombine};
    ///
    /// // What a `route()` answers for `ZINTERSTORE dst k1 k2`.
    /// let route = Route::ZAlgebraStore(ZCombine::ZInter);
    /// assert!(matches!(route, Route::ZAlgebraStore(ZCombine::ZInter)));
    /// ```
    ZInter,
    /// `ZUNIONSTORE`.
    ///
    /// ```
    /// use kevy_rt::{Route, ZCombine};
    ///
    /// // What a `route()` answers for `ZUNIONSTORE dst k1 k2`.
    /// let route = Route::ZAlgebraStore(ZCombine::ZUnion);
    /// assert!(matches!(route, Route::ZAlgebraStore(ZCombine::ZUnion)));
    /// ```
    ZUnion,
    /// `ZDIFFSTORE`.
    ///
    /// ```
    /// use kevy_rt::{Route, ZCombine};
    ///
    /// // What a `route()` answers for `ZDIFFSTORE dst k1 k2`.
    /// let route = Route::ZAlgebraStore(ZCombine::ZDiff);
    /// assert!(matches!(route, Route::ZAlgebraStore(ZCombine::ZDiff)));
    /// ```
    ZDiff,
    /// `SINTERSTORE`.
    ///
    /// ```
    /// use kevy_rt::{Route, ZCombine};
    ///
    /// // What a `route()` answers for `SINTERSTORE dst k1 k2`.
    /// let route = Route::ZAlgebraStore(ZCombine::SInter);
    /// assert!(matches!(route, Route::ZAlgebraStore(ZCombine::SInter)));
    /// ```
    SInter,
    /// `SUNIONSTORE`.
    ///
    /// ```
    /// use kevy_rt::{Route, ZCombine};
    ///
    /// // What a `route()` answers for `SUNIONSTORE dst k1 k2`.
    /// let route = Route::ZAlgebraStore(ZCombine::SUnion);
    /// assert!(matches!(route, Route::ZAlgebraStore(ZCombine::SUnion)));
    /// ```
    SUnion,
    /// `SDIFFSTORE`.
    ///
    /// ```
    /// use kevy_rt::{Route, ZCombine};
    ///
    /// // What a `route()` answers for `SDIFFSTORE dst k1 k2`.
    /// let route = Route::ZAlgebraStore(ZCombine::SDiff);
    /// assert!(matches!(route, Route::ZAlgebraStore(ZCombine::SDiff)));
    /// ```
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
