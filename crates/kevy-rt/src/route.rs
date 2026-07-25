//! [`Route`] — how each command maps onto shards. Returned by
//! [`crate::Commands::route`] / carried in [`crate::ResolvedCmd`]; the
//! runtime's `start_command` matches on it to pick a dispatch shape.

use crate::exec_slowlog::SlowlogSub;

/// How a command maps onto shards.
#[derive(Debug, PartialEq)]
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
    /// Geo `*STORE` family — `GEOSEARCHSTORE dst src …` and
    /// `GEORADIUS[BYMEMBER] src … STORE|STOREDIST dst`.
    ///
    /// These MUST be routed, not left to the catch-all `Route::Single(1)`:
    /// GEOSEARCHSTORE puts the DESTINATION at argv[1] (so the search then
    /// read the source off the wrong shard — `:0`, or "could not decode
    /// requested zset member" for FROMMEMBER) while GEORADIUS puts the
    /// SOURCE there (so the destination was written into the source's
    /// shard, invisible to every later read of it). Both keys are carried
    /// here because neither sits at a fixed argv index — the legacy forms
    /// hide `dst` behind an option-soup scan.
    ///
    /// The search runs on `src`'s shard ([`crate::Commands::geo_search`]),
    /// the write lands on `dst`'s (`Op::ZStoreResult`) — see
    /// [`crate::exec_geostore`].
    GeoStore { src: Vec<u8>, dst: Vec<u8> },
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
    ReplWait { numreplicas: u32, timeout_ms: u64 },
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
        offsets: Vec<u64>,
        timeout_ms: u64,
        miss: Vec<u8>,
    },
    /// `KEYS pattern` — every shard returns its matching keys.
    Keys(Option<Vec<u8>>),
    /// `SCAN cursor [MATCH pattern] [COUNT count] [TYPE type]` — a real
    /// cursor iterator: each call visits ~COUNT buckets of ONE shard
    /// (chaining into the next shard only while the work budget lasts)
    /// and replies `[next-cursor, keys]`. `Err` carries the pre-parsed
    /// error message the command layer wants on the wire (invalid
    /// cursor / syntax error) — the runtime replies it verbatim.
    Scan(Result<ScanArgs, &'static str>),
    /// `RANDOMKEY` — one arbitrary key across all shards.
    RandomKey,
    /// `SUBSCRIBE` / `UNSUBSCRIBE` — connection-level (modifies this conn).
    Subscribe,
    Unsubscribe,
    /// `PSUBSCRIBE pattern [pattern ...]` / `PUNSUBSCRIBE [pattern ...]` —
    /// like Subscribe/Unsubscribe but the conn registers Redis-glob
    /// patterns; `PUBLISH` to a matching channel delivers a `pmessage`
    /// frame. Connection-level (modifies this conn + shared pattern
    /// registry).
    Psubscribe,
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
    /// the catch-all route hashes args[1] (the source), so the destination
    /// push executed on the SOURCE's shard and the element was written into
    /// a keyspace nobody would ever read it from. It returned the moved
    /// value, so the caller believed it had worked. Measured on an 8-shard
    /// server: 11 of 12 moves silently lost the element.
    ///
    /// Same-shard pairs are one atomic Op on the owning shard. Cross-shard
    /// pairs run the Take→Push orchestrator (mirroring [`Self::Rename`]),
    /// which is NOT atomic — see `exec_listmove`.
    ListMove {
        /// Pop from the head of the source (`LMOVE ... LEFT ...`) rather
        /// than the tail (`RPOPLPUSH`).
        from_left: bool,
        /// Push onto the head of the destination (`RPOPLPUSH`, `LMOVE ...
        /// LEFT`) rather than the tail.
        to_left: bool,
    },
    /// `SLOWLOG GET / LEN / RESET / HELP`. The sub-command + parsed
    /// args are pre-decoded at routing time so the runtime knows
    /// whether to short-circuit (HELP / error) or fan out across
    /// shards (GET / LEN / RESET). See [`crate::parse_slowlog_sub`].
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
        streams: Vec<(Vec<u8>, Vec<u8>)>,
        count: Option<usize>,
        group: Option<XGroupCtx>,
    },
}

/// Parsed `SCAN` arguments carried by [`Route::Scan`].
///
/// `cursor` is the raw wire cursor: the runtime splits it into
/// `(shard, in-shard position)` — shard index in the top 10 bits,
/// reverse-binary bucket cursor in the low 54 (see `exec_scan` for the
/// documented limits). Cursors are therefore only meaningful on the
/// server (and shard count) that issued them, like Redis Cluster
/// cursors are per-node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanArgs {
    /// Raw wire cursor (`0` starts a sweep).
    pub cursor: u64,
    /// `COUNT` — buckets-visited work bound per call (default 10).
    pub count: usize,
    /// `MATCH` glob, applied to each visited key.
    pub pattern: Option<Vec<u8>>,
    /// `TYPE` — keep only keys whose value type name matches
    /// (case-insensitive; unknown names match nothing).
    pub type_filter: Option<Vec<u8>>,
}

/// The `GROUP <name> <consumer>` (+ `NOACK`) context an `XREADGROUP`
/// gather carries to each per-stream sub-query.
#[derive(Debug, PartialEq)]
pub struct XGroupCtx {
    /// Consumer-group name.
    pub group: Vec<u8>,
    /// Consumer name within the group.
    pub consumer: Vec<u8>,
    /// `NOACK` — deliver without adding to the PEL.
    pub noack: bool,
}
