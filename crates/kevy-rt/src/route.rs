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
    Local,
    /// Single-key; route by `args[idx]`.
    Single(usize),
    /// `args[1..]` are keys; delete each on its shard, sum the counts.
    DelKeys,
    /// `args[1..]` are keys; count existing across shards.
    ExistsKeys,
    /// Sum every shard's key count.
    Dbsize,
    /// Flush every shard.
    Flush,
    /// Snapshot every shard's store to disk, synchronously (`SAVE` —
    /// blocks until durable, the Redis contract for the explicit form).
    Save,
    /// `BGSAVE` — collect a COW view per shard and persist in the
    /// background; the command returns once the views are frozen.
    BgSave,
    /// `BGREWRITEAOF` — rebuild every shard's AOF from in-memory state.
    /// Each shard freezes a COW view and hands the dump to its persist
    /// worker, so the reply returns before the rewrite is durable.
    RewriteAof,
    /// `MSET` — `args[1..]` are key/value pairs, routed per key's shard.
    MSet,
    /// Cross-shard multi-key gather (`MGET` / `SINTER` / `SUNION` /
    /// `SDIFF` / `ZINTERCARD`): each key's payload is fetched on its
    /// owning shard and the origin reduces them per [`crate::MultiOp`].
    Gather(crate::MultiOp),
    /// zset/set algebra `*STORE` family: gather sources, combine
    /// per [`crate::message::ZCombine`], materialize at `args[1]`.
    ZAlgebraStore(crate::ZCombine),
    /// `BITOP op dst src [src …]` — N sources gathered, combined, and
    /// stored at a destination that sits at `args[2]`, not `args[1]`.
    /// `ZAlgebraStore` is the same shape with a different payload: it
    /// combines set and zset members, not raw bytes.
    ///
    /// Carries nothing. An earlier draft carried the operator so the
    /// router could pick it, which meant parsing the operator twice and
    /// needing a fallback route for the argv the router could not parse
    /// — and that fallback led to a dispatch table with no BITOP arm,
    /// so a malformed BITOP would have been answered "unknown command".
    /// The route says only that this is a BITOP; every refusal is
    /// worded once, in `exec_bitop`.
    ///
    /// Why it cannot ride `Single(1)`, in one assertion:
    ///
    /// ```
    /// use kevy_persist::Routing;
    /// use kevy_rt::{Route, shard_of_key};
    /// // `Single(1)` hashes args[1]. For BITOP that is the OPERATOR.
    /// let operator = b"AND".as_slice();
    /// let destination = b"dst".as_slice();
    /// assert_ne!(shard_of_key(operator, 8, Routing::KevyHash), shard_of_key(destination, 8, Routing::KevyHash));
    /// assert!(matches!(Route::BitOpStore, Route::BitOpStore));
    /// ```
    BitOpStore,
    /// `COPY src dst [REPLACE]` — two keys, so the same hazard the
    /// rename and list-move routes exist for: left to the catch-all
    /// `Single(1)` the copy lands in the SOURCE's shard, where no later
    /// read of the destination will ever look. Same-shard pairs take
    /// one atomic op; cross-shard pairs run Read → Put, and need no
    /// rollback because the read does not remove anything.
    ///
    /// Why it cannot ride `Single(1)`, in one assertion:
    ///
    /// ```
    /// use kevy_persist::Routing;
    /// use kevy_rt::{Route, shard_of_key};
    /// // A pair of ordinary key names on an eight-shard server.
    /// let (src, dst) = (b"ca".as_slice(), b"cb".as_slice());
    /// assert_ne!(shard_of_key(src, 8, Routing::KevyHash), shard_of_key(dst, 8, Routing::KevyHash));
    /// // `Single(1)` hashes args[1] — the SOURCE — and runs the whole
    /// // command there, so the copy would land in a shard no later read
    /// // of `dst` ever looks at, while the reply said it worked.
    /// assert!(matches!(Route::Copy, Route::Copy));
    /// ```
    Copy,
    /// Geo `*STORE` family — `GEOSEARCHSTORE dst src …` and
    /// `GEORADIUS[BYMEMBER] src … STORE|STOREDIST dst`.
    ///
    /// These MUST be routed, not left to the catch-all `Route::Single(1)`:
    /// GEOSEARCHSTORE puts the DESTINATION at `argv[1]` (so the search then
    /// read the source off the wrong shard — `:0`, or "could not decode
    /// requested zset member" for FROMMEMBER) while GEORADIUS puts the
    /// SOURCE there (so the destination was written into the source's
    /// shard, invisible to every later read of it). Both keys are carried
    /// here because neither sits at a fixed argv index — the legacy forms
    /// hide `dst` behind an option-soup scan.
    ///
    /// The search runs on `src`'s shard ([`crate::Commands::geo_search`]),
    /// the write lands on `dst`'s (`Op::ZStoreResult`) — see
    /// the runtime's geo-store orchestration.
    GeoStore {
        /// Key the search reads — its shard runs the query.
        src: Vec<u8>,
        /// Key the result is written to — its shard takes the write, which
        /// is why both keys have to be extracted before routing.
        dst: Vec<u8>,
    },
    /// `FEED.READ <shard> <gen> <offset> …` — shard-index routed.
    FeedRead,
    /// `FEED.TAIL <shard>`.
    FeedTail,
    /// `FEED.SHARDS` — answered locally.
    FeedShards,
    /// `PREFIX.STATS <prefix>` — all-shard fanout, summed.
    PrefixStats,
    /// `CLIENT LIST` — all-shard fanout; each shard renders its conn
    /// table rows, the origin concatenates into one bulk reply.
    ClientList,
    /// `CLIENT KILL …` — all-shard fanout; each shard closes its
    /// matching conns, the origin sums (or maps the legacy positional
    /// form to `+OK` / `-ERR`).
    ClientKill,
    /// Extension fan-out (IDX.* reads): every shard runs
    /// `Commands::extension_op`, the origin reduces.
    Extension,
    /// `WAIT numreplicas timeout` — all-shard barrier: each
    /// shard answers (possibly deferred until its replicas ACK or the
    /// deadline) with how many of its replicas acked its
    /// `master_repl_offset` at arm time; the origin replies the MIN.
    /// `timeout_ms == 0` = the Redis "wait forever" form (the runtime
    /// hard-caps it — see `exec_replwait::WAIT_HARD_CAP_MS`).
    ReplWait {
        /// How many replicas the caller wants acked. Reported per shard;
        /// the origin answers the minimum across them.
        numreplicas: u32,
        /// Deadline in milliseconds. `0` is Redis's wait-forever form and
        /// is hard-capped by the runtime rather than honoured literally.
        timeout_ms: u64,
    },
    /// `REPL.TOKEN` on a primary — gather every shard's
    /// `(feed generation, next_offset)` pair into one flat array.
    ReplToken,
    /// `REPL.WAIT` on a replica — all-shard applied barrier:
    /// shard `i` answers once its replication-apply position reaches
    /// `offsets[i]` (or the deadline passes). All met → `+OK`; any
    /// timeout → the pre-built `miss` reply (kevy sends
    /// `-MISDIRECTED writer is <primary>`). The command layer builds
    /// `miss` because the upstream address is its knowledge, not the
    /// runtime's.
    ReplBarrier {
        /// One target apply-position per shard, indexed by shard number.
        offsets: Vec<u64>,
        /// Deadline in milliseconds for every shard to reach its target.
        timeout_ms: u64,
        /// The reply to send if any shard misses its deadline, pre-built by
        /// the command layer because it names the upstream primary — the
        /// runtime does not know that address.
        miss: Vec<u8>,
    },
    /// `KEYS pattern` — every shard returns its matching keys.
    Keys(Option<Vec<u8>>),
    /// `SCAN cursor [MATCH pattern] [COUNT count] [TYPE type]` — a real
    /// cursor iterator: each call visits ~COUNT buckets of ONE shard
    /// (chaining into the next shard only while the work budget lasts)
    /// and replies `[next-cursor, keys]`. `Err` carries why the command
    /// layer refused the arguments (invalid cursor / syntax error); the
    /// runtime replies its wire form.
    ///
    /// The cursor is the raw wire cursor: the runtime splits it into
    /// `(shard, in-shard position)` — shard index in the top 10 bits,
    /// reverse-binary bucket cursor in the low 54. Cursors are therefore
    /// only meaningful on the server (and shard count) that issued them,
    /// like Redis Cluster cursors are per-node.
    Scan(Result<ScanOpts, ScanOptsError>),
    /// `RANDOMKEY` — one arbitrary key across all shards.
    RandomKey,
    /// `SUBSCRIBE` / `UNSUBSCRIBE` — connection-level (modifies this conn).
    Subscribe,
    /// The other half of the pair above: drops this conn's channel
    /// subscriptions, all of them when no channel is named.
    Unsubscribe,
    /// `PSUBSCRIBE pattern [pattern ...]` / `PUNSUBSCRIBE [pattern ...]` —
    /// like Subscribe/Unsubscribe but the conn registers Redis-glob
    /// patterns; `PUBLISH` to a matching channel delivers a `pmessage`
    /// frame. Connection-level (modifies this conn + shared pattern
    /// registry).
    Psubscribe,
    /// The other half of the pattern pair: drops this conn's pattern
    /// subscriptions, all of them when no pattern is named, and removes
    /// them from the shared registry.
    Punsubscribe,
    /// `PUBLISH channel message` — delivered to subscribers on every core.
    Publish,
    /// `WATCH key [key ...]` — fan-out to record per-shard versions, then
    /// stash the (key, version) pairs in the conn's `watched` set so the
    /// next `EXEC` can validate them. Connection-level.
    Watch,
    /// `UNWATCH` — clear the conn's `watched` set. Connection-level, local.
    Unwatch,
    /// `HELLO [protover [AUTH user pass] [SETNAME name]]` — server
    /// handshake; on `HELLO 3` flips the conn into RESP3 mode (per-conn
    /// `proto` field). Reply shape itself is proto-aware (V2: array of
    /// pairs; V3: Map). Connection-level, dispatch via the
    /// [`crate::Commands::hello_reply`] hook so embedders set their own server
    /// metadata.
    Hello,
    /// `RENAME source destination` / `RENAMENX source destination`. The
    /// runtime handles the two-shard decision: same-shard renames go
    /// through one atomic [`crate::Store::rename`] on the owning shard; cross-
    /// shard renames use the Take→Put orchestrator (lands in v2-3b;
    /// v2-3a emits `-CROSSSHARD ...` for that case).
    Rename {
        /// `true` for `RENAMENX` (no overwrite — reply `:0` if dst exists).
        nx: bool,
    },
    /// `RPOPLPUSH src dst` / `LMOVE src dst LEFT|RIGHT LEFT|RIGHT` /
    /// `BRPOPLPUSH src dst timeout`, once the blocking form has an element
    /// to serve.
    ///
    /// These MUST be routed, not left to `Route::Single(1)`. The source and
    /// the destination are different keys and can live on different shards;
    /// the catch-all route hashes `args[1]` (the source), so the destination
    /// push executed on the SOURCE's shard and the element was written into
    /// a keyspace nobody would ever read it from. It returned the moved
    /// value, so the caller believed it had worked. Measured on an 8-shard
    /// server: 11 of 12 moves silently lost the element.
    ///
    /// Same-shard pairs are one atomic Op on the owning shard. Cross-shard
    /// pairs run the Take→Push orchestrator (mirroring [`Self::Rename`]),
    /// which is NOT atomic — see `exec_listmove`.
    ListMove {
        /// The end of the source to pop from: the head for
        /// `LMOVE ... LEFT ...`, the tail for `RPOPLPUSH`.
        from: ListEnd,
        /// The end of the destination to push onto: the head for
        /// `RPOPLPUSH` and `LMOVE ... LEFT`, else the tail.
        to: ListEnd,
    },
    /// `SLOWLOG GET / LEN / RESET / HELP`. The sub-command + parsed
    /// args are pre-decoded at routing time so the runtime knows
    /// whether to short-circuit (HELP / error) or fan out across
    /// shards (GET / LEN / RESET). See [`SlowlogSub::parse`].
    Slowlog(SlowlogSub),
    /// Non-blocking `XREAD` / `XREADGROUP` over **multiple** streams — fan
    /// each stream out to its owning shard and merge the per-stream replies
    /// in request order (single-stream forms still route via
    /// [`Self::Single`]). Each element is `(stream key, last-seen id)`;
    /// `count` is the optional `COUNT` cap applied per stream; `group`
    /// `Some` makes each per-shard sub-query an `XREADGROUP` (a write —
    /// PEL / last-delivered updates happen on each stream's owning shard
    /// and are AOF-logged there as the rewritten single-stream command).
    /// The command set builds this only for the non-blocking, ≥2-stream
    /// forms; blocking reads park on the origin shard instead (see the
    /// cross-shard BLOCK arbiter).
    XReadGather {
        /// `(stream key, start id)` per stream, already paired — the wire
        /// form lists all keys and then all ids, which is not routable.
        streams: Vec<(Vec<u8>, Vec<u8>)>,
        /// `COUNT`, applied per stream rather than across the gather.
        count: Option<usize>,
        /// `Some` turns each per-shard sub-query into an XREADGROUP, which
        /// makes it a write: the PEL update happens on the stream's own
        /// shard and is logged there.
        group: Option<XGroupCtx>,
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
    pub group: Vec<u8>,
    /// Consumer name within the group.
    pub consumer: Vec<u8>,
    /// Whether deliveries enter the pending list (`NOACK` = they do not).
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
