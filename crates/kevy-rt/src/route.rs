//! [`Route`] — how each command maps onto shards. Returned by
//! [`crate::Commands::route`] / carried in [`crate::ResolvedCmd`]; the
//! runtime's `start_command` matches on it to pick a dispatch shape.

use crate::exec_slowlog::SlowlogSub;
use kevy_store::AckMode;
use kevy_store::ListEnd;
use kevy_verbs::args::{ScanOpts, ScanOptsError};

/// How a command maps onto shards.
///
/// Commands grow new shapes over releases, so the set is open: a
/// [`crate::Commands`] implementation constructs the variants it needs,
/// and code outside this crate that matches a route keeps a wildcard arm.
///
/// ```
/// use kevy_rt::Route;
///
/// let get = Route::Single(1);
/// assert_ne!(get, Route::Local);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Route {
    /// Keyless; execute on the connection's own shard (e.g. PING).
    ///
    /// ```
    /// # use kevy_rt::{Route};
    /// let route = |verb: &str| if verb == "PING" { Route::Local } else { Route::Single(1) };
    /// assert_eq!(route("PING"), Route::Local);
    /// ```
    Local,
    #[doc = include_str!("route_docs/single.md")]
    Single(usize),
    /// `args[1..]` are keys; delete each on its shard, sum the counts.
    ///
    /// ```
    /// # use kevy_rt::{Route};
    /// let route = |verb: &str| if verb == "DEL" { Route::DelKeys } else { Route::Single(1) };
    /// assert_eq!(route("DEL"), Route::DelKeys);
    /// ```
    DelKeys,
    /// `args[1..]` are keys; count existing across shards.
    ///
    /// ```
    /// # use kevy_rt::{Route};
    /// let route = |verb: &str| if verb == "EXISTS" { Route::ExistsKeys } else { Route::Single(1) };
    /// assert_eq!(route("EXISTS"), Route::ExistsKeys);
    /// ```
    ExistsKeys,
    /// Sum every shard's key count.
    ///
    /// ```
    /// # use kevy_rt::{Route};
    /// let route = |verb: &str| if verb == "DBSIZE" { Route::Dbsize } else { Route::Local };
    /// assert_eq!(route("DBSIZE"), Route::Dbsize);
    /// ```
    Dbsize,
    /// Flush every shard.
    ///
    /// ```
    /// # use kevy_rt::{Route};
    /// let route = |verb: &str| if verb == "FLUSHALL" { Route::Flush } else { Route::Local };
    /// assert_eq!(route("FLUSHALL"), Route::Flush);
    /// ```
    Flush,
    /// Snapshot every shard's store to disk, synchronously (`SAVE` —
    /// blocks until durable, the Redis contract for the explicit form).
    ///
    /// ```
    /// # use kevy_rt::{Route};
    /// let route = |verb: &str| if verb == "SAVE" { Route::Save } else { Route::Local };
    /// assert_eq!(route("SAVE"), Route::Save);
    /// ```
    Save,
    /// `BGSAVE` — collect a COW view per shard and persist in the
    /// background; the command returns once the views are frozen.
    ///
    /// ```
    /// # use kevy_rt::{Route};
    /// let route = |verb: &str| if verb == "BGSAVE" { Route::BgSave } else { Route::Local };
    /// assert_eq!(route("BGSAVE"), Route::BgSave);
    /// ```
    BgSave,
    /// `BGREWRITEAOF` — rebuild every shard's AOF from in-memory state.
    /// Each shard freezes a COW view and hands the dump to its persist
    /// worker, so the reply returns before the rewrite is durable.
    ///
    /// ```
    /// # use kevy_rt::{Route};
    /// let route = |verb: &str| if verb == "BGREWRITEAOF" { Route::RewriteAof } else { Route::Local };
    /// assert_eq!(route("BGREWRITEAOF"), Route::RewriteAof);
    /// ```
    RewriteAof,
    /// `MSET` — `args[1..]` are key/value pairs, routed per key's shard.
    ///
    /// ```
    /// # use kevy_rt::{Route};
    /// let route = |verb: &str| if verb == "MSET" { Route::MSet } else { Route::Single(1) };
    /// assert_eq!(route("MSET"), Route::MSet);
    /// ```
    MSet,
    /// Cross-shard multi-key gather (`MGET` / `SINTER` / `SUNION` /
    /// `SDIFF` / `ZINTERCARD`): each key's payload is fetched on its
    /// owning shard and the origin reduces them per [`crate::MultiOp`].
    ///
    /// ```
    /// # use kevy_rt::{MultiOp, Route};
    /// let route = |verb: &str| if verb == "MGET" { Route::Gather(MultiOp::Mget) } else { Route::Single(1) };
    /// assert_eq!(route("MGET"), Route::Gather(MultiOp::Mget));
    /// ```
    Gather(crate::MultiOp),
    /// zset/set algebra `*STORE` family: gather sources, combine
    /// per [`crate::message::ZCombine`], materialize at `args[1]`.
    ///
    /// ```
    /// # use kevy_rt::{Route, ZCombine};
    /// let route = |verb: &str| if verb == "ZUNIONSTORE" { Route::ZAlgebraStore(ZCombine::ZUnion) } else { Route::Single(1) };
    /// assert_eq!(route("ZUNIONSTORE"), Route::ZAlgebraStore(ZCombine::ZUnion));
    /// ```
    ZAlgebraStore(crate::ZCombine),
    #[doc = include_str!("route_docs/bitopstore.md")]
    BitOpStore,
    #[doc = include_str!("route_docs/copy.md")]
    Copy,
    #[doc = include_str!("route_docs/geostore.md")]
    GeoStore {
        /// Key the search reads — its shard runs the query.
        ///
        /// ```
        /// use kevy_rt::Route;
        ///
        /// // `GEORADIUS cities ... STORE near`: `cities` is searched.
        /// let route = Route::GeoStore { src: b"cities".to_vec(), dst: b"near".to_vec() };
        /// assert!(matches!(route, Route::GeoStore { ref src, .. } if src == b"cities"));
        /// ```
        src: Vec<u8>,
        #[doc = include_str!("route_docs/geostore_dst.md")]
        dst: Vec<u8>,
    },
    /// `FEED.READ <shard> <gen> <offset> …` — shard-index routed.
    ///
    /// ```
    /// # use kevy_rt::{Route};
    /// let route = |verb: &str| if verb == "FEED.READ" { Route::FeedRead } else { Route::Single(1) };
    /// assert_eq!(route("FEED.READ"), Route::FeedRead);
    /// ```
    FeedRead,
    /// `FEED.TAIL <shard>`.
    ///
    /// ```
    /// # use kevy_rt::{Route};
    /// let route = |verb: &str| if verb == "FEED.TAIL" { Route::FeedTail } else { Route::Single(1) };
    /// assert_eq!(route("FEED.TAIL"), Route::FeedTail);
    /// ```
    FeedTail,
    /// `FEED.SHARDS` — answered locally.
    ///
    /// ```
    /// # use kevy_rt::{Route};
    /// let route = |verb: &str| if verb == "FEED.SHARDS" { Route::FeedShards } else { Route::Single(1) };
    /// assert_eq!(route("FEED.SHARDS"), Route::FeedShards);
    /// ```
    FeedShards,
    /// `PREFIX.STATS <prefix>` — all-shard fanout, summed.
    ///
    /// ```
    /// # use kevy_rt::{Route};
    /// let route = |verb: &str| if verb == "PREFIX.STATS" { Route::PrefixStats } else { Route::Single(1) };
    /// assert_eq!(route("PREFIX.STATS"), Route::PrefixStats);
    /// ```
    PrefixStats,
    /// `CLIENT LIST` — all-shard fanout; each shard renders its conn
    /// table rows, the origin concatenates into one bulk reply.
    ///
    /// ```
    /// use kevy_rt::Route;
    ///
    /// let route = |sub: &str| if sub == "LIST" { Route::ClientList } else { Route::Local };
    /// assert_eq!(route("LIST"), Route::ClientList);
    /// ```
    ClientList,
    #[doc = include_str!("route_docs/clientkill.md")]
    ClientKill,
    /// Extension fan-out (IDX.* reads): every shard runs
    /// `Commands::extension_op`, the origin reduces.
    ///
    /// ```
    /// # use kevy_rt::{Route};
    /// let route = |verb: &str| if verb == "IDX.SEARCH" { Route::Extension } else { Route::Single(1) };
    /// assert_eq!(route("IDX.SEARCH"), Route::Extension);
    /// ```
    Extension,
    #[doc = include_str!("route_docs/replwait.md")]
    ReplWait {
        #[doc = include_str!("route_docs/replwait_numreplicas.md")]
        numreplicas: u32,
        #[doc = include_str!("route_docs/replwait_timeout_ms.md")]
        timeout_ms: u64,
    },
    /// `REPL.TOKEN` on a primary — gather every shard's
    /// `(feed generation, next_offset)` pair into one flat array.
    ///
    /// ```
    /// # use kevy_rt::{Route};
    /// let route = |verb: &str| if verb == "REPL.TOKEN" { Route::ReplToken } else { Route::Local };
    /// assert_eq!(route("REPL.TOKEN"), Route::ReplToken);
    /// ```
    ReplToken,
    #[doc = include_str!("route_docs/replbarrier.md")]
    ReplBarrier {
        /// One target apply-position per shard, indexed by shard number.
        ///
        /// ```
        /// use kevy_rt::Route;
        ///
        /// // Shard 1 must have applied up to offset 7.
        /// let route = Route::ReplBarrier { offsets: vec![10, 7], timeout_ms: 100, miss: Vec::new() };
        /// assert!(matches!(route, Route::ReplBarrier { ref offsets, .. } if offsets[1] == 7));
        /// ```
        offsets: Vec<u64>,
        /// Deadline in milliseconds for every shard to reach its target.
        ///
        /// ```
        /// use kevy_rt::Route;
        ///
        /// let route = Route::ReplBarrier { offsets: vec![0], timeout_ms: 250, miss: Vec::new() };
        /// assert!(matches!(route, Route::ReplBarrier { timeout_ms: 250, .. }));
        /// ```
        timeout_ms: u64,
        #[doc = include_str!("route_docs/replbarrier_miss.md")]
        miss: Vec<u8>,
    },
    /// `KEYS pattern` — every shard returns its matching keys.
    ///
    /// ```
    /// use kevy_rt::Route;
    ///
    /// // `KEYS user:*`
    /// let route = Route::Keys(Some(b"user:*".to_vec()));
    /// assert!(matches!(route, Route::Keys(Some(ref p)) if p == b"user:*"));
    /// ```
    Keys(Option<Vec<u8>>),
    #[doc = include_str!("route_docs/scan.md")]
    Scan(Result<ScanOpts, ScanOptsError>),
    /// `RANDOMKEY` — one arbitrary key across all shards.
    ///
    /// ```
    /// # use kevy_rt::{Route};
    /// let route = |verb: &str| if verb == "RANDOMKEY" { Route::RandomKey } else { Route::Local };
    /// assert_eq!(route("RANDOMKEY"), Route::RandomKey);
    /// ```
    RandomKey,
    /// `SUBSCRIBE` / `UNSUBSCRIBE` — connection-level (modifies this conn).
    ///
    /// ```
    /// # use kevy_rt::{Route};
    /// let route = |verb: &str| if verb == "SUBSCRIBE" { Route::Subscribe } else { Route::Single(1) };
    /// assert_eq!(route("SUBSCRIBE"), Route::Subscribe);
    /// ```
    Subscribe,
    /// The other half of the pair above: drops this conn's channel
    /// subscriptions, all of them when no channel is named.
    ///
    /// ```
    /// # use kevy_rt::{Route};
    /// let route = |verb: &str| if verb == "UNSUBSCRIBE" { Route::Unsubscribe } else { Route::Single(1) };
    /// assert_eq!(route("UNSUBSCRIBE"), Route::Unsubscribe);
    /// ```
    Unsubscribe,
    #[doc = include_str!("route_docs/psubscribe.md")]
    Psubscribe,
    /// The other half of the pattern pair: drops this conn's pattern
    /// subscriptions, all of them when no pattern is named, and removes
    /// them from the shared registry.
    ///
    /// ```
    /// # use kevy_rt::{Route};
    /// let route = |verb: &str| if verb == "PUNSUBSCRIBE" { Route::Punsubscribe } else { Route::Single(1) };
    /// assert_eq!(route("PUNSUBSCRIBE"), Route::Punsubscribe);
    /// ```
    Punsubscribe,
    /// `PUBLISH channel message` — delivered to subscribers on every core.
    ///
    /// ```
    /// # use kevy_rt::{Route};
    /// let route = |verb: &str| if verb == "PUBLISH" { Route::Publish } else { Route::Single(1) };
    /// assert_eq!(route("PUBLISH"), Route::Publish);
    /// ```
    Publish,
    /// `WATCH key [key ...]` — fan-out to record per-shard versions, then
    /// stash the (key, version) pairs in the conn's `watched` set so the
    /// next `EXEC` can validate them. Connection-level.
    ///
    /// ```
    /// # use kevy_rt::{Route};
    /// let route = |verb: &str| if verb == "WATCH" { Route::Watch } else { Route::Single(1) };
    /// assert_eq!(route("WATCH"), Route::Watch);
    /// ```
    Watch,
    /// `UNWATCH` — clear the conn's `watched` set. Connection-level, local.
    ///
    /// ```
    /// # use kevy_rt::{Route};
    /// let route = |verb: &str| if verb == "UNWATCH" { Route::Unwatch } else { Route::Local };
    /// assert_eq!(route("UNWATCH"), Route::Unwatch);
    /// ```
    Unwatch,
    #[doc = include_str!("route_docs/hello.md")]
    Hello,
    #[doc = include_str!("route_docs/rename.md")]
    Rename {
        /// `true` for `RENAMENX` (no overwrite — reply `:0` if dst exists).
        ///
        /// ```
        /// use kevy_rt::Route;
        ///
        /// // `RENAMENX a b` refuses to overwrite `b`.
        /// let route = |verb: &str| Route::Rename { nx: verb == "RENAMENX" };
        /// assert_eq!(route("RENAMENX"), Route::Rename { nx: true });
        /// ```
        nx: bool,
    },
    #[doc = include_str!("route_docs/listmove.md")]
    ListMove {
        #[doc = include_str!("route_docs/listmove_from.md")]
        from: ListEnd,
        #[doc = include_str!("route_docs/listmove_to.md")]
        to: ListEnd,
    },
    #[doc = include_str!("route_docs/slowlog.md")]
    Slowlog(SlowlogSub),
    #[doc = include_str!("route_docs/xreadgather.md")]
    XReadGather {
        #[doc = include_str!("route_docs/xreadgather_streams.md")]
        streams: Vec<(Vec<u8>, Vec<u8>)>,
        #[doc = include_str!("route_docs/xreadgather_count.md")]
        count: Option<usize>,
        #[doc = include_str!("route_docs/xreadgather_group.md")]
        group: Option<XGroupCtx>,
    },
    /// A pop naming `numkeys` keys from argument 2 on, the count itself at
    /// argument 1, that takes from the first key holding something —
    /// `ZMPOP` / `LMPOP`. Keys on one shard run there as sent, atomically;
    /// keys apart are tried in order, each as the same command naming that
    /// key alone, and the first reply that is not null answers.
    ///
    /// ```
    /// use kevy_rt::Route;
    ///
    /// // `ZMPOP 2 a b MIN`: two keys at arguments 2 and 3.
    /// let route = Route::FirstHit { numkeys: 2 };
    /// assert_ne!(route, Route::Single(2));
    /// ```
    FirstHit {
        /// How many keys follow the count.
        numkeys: usize,
    },
    /// A read naming `count` keys from argument `first` on, answered from
    /// one computation over them — `LCS`, `SINTERCARD`, `ZINTER`. Keys on
    /// one shard run there as sent; keys apart are copied to the shard the
    /// command came to and the command runs over the copies.
    ///
    /// ```
    /// use kevy_rt::Route;
    ///
    /// // `LCS a b`: two keys from argument 1.
    /// let route = Route::ReadAcross { first: 1, count: 2 };
    /// assert_ne!(route, Route::Single(1));
    /// ```
    ReadAcross {
        /// Where the keys start.
        first: usize,
        /// How many there are.
        count: usize,
    },
}

/// The `GROUP <name> <consumer>` (+ `NOACK`) context an `XREADGROUP`
/// gather carries to each per-stream sub-query.
///
/// ```
/// use kevy_rt::XGroupCtx;
/// use kevy_store::AckMode;
///
/// let ctx = XGroupCtx::new(b"workers".to_vec(), b"w1".to_vec()).with_ack(AckMode::NoAck);
/// assert_eq!(ctx.ack, AckMode::NoAck);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct XGroupCtx {
    /// Consumer-group name.
    ///
    /// ```
    /// let ctx = kevy_rt::XGroupCtx::new(b"workers".to_vec(), b"w1".to_vec());
    /// assert_eq!(ctx.group, b"workers");
    /// ```
    pub group: Vec<u8>,
    /// Consumer name within the group.
    ///
    /// ```
    /// let ctx = kevy_rt::XGroupCtx::new(b"workers".to_vec(), b"w1".to_vec());
    /// assert_eq!(ctx.consumer, b"w1");
    /// ```
    pub consumer: Vec<u8>,
    /// Whether deliveries enter the pending list (`NOACK` = they do not).
    ///
    /// ```
    /// use kevy_store::AckMode;
    ///
    /// // `XREADGROUP ... NOACK`: deliveries skip the pending list.
    /// let ctx = kevy_rt::XGroupCtx::new(b"g".to_vec(), b"c".to_vec()).with_ack(AckMode::NoAck);
    /// assert_eq!(ctx.ack, AckMode::NoAck);
    /// ```
    pub ack: AckMode,
}

impl XGroupCtx {
    /// Read as `consumer` of `group`, adding deliveries to the pending
    /// list (no `NOACK`).
    ///
    /// ```
    /// let ctx = kevy_rt::XGroupCtx::new(b"g".to_vec(), b"c".to_vec());
    /// assert_eq!(ctx.ack, kevy_store::AckMode::Pending);
    /// ```
    #[inline]
    #[must_use]
    pub fn new(group: Vec<u8>, consumer: Vec<u8>) -> Self {
        Self { group, consumer, ack: AckMode::Pending }
    }

    /// Set [`Self::ack`].
    ///
    /// ```
    /// use kevy_store::AckMode;
    /// let ctx = kevy_rt::XGroupCtx::new(b"g".to_vec(), b"c".to_vec()).with_ack(AckMode::NoAck);
    /// assert_eq!(ctx.ack, AckMode::NoAck);
    /// ```
    #[inline]
    #[must_use]
    pub fn with_ack(mut self, ack: AckMode) -> Self {
        self.ack = ack;
        self
    }
}
