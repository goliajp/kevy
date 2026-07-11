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
    /// Keyspace collection (`KEYS` / `SCAN` / `RANDOMKEY`) — every
    /// shard contributes its matching keys, shaped at the origin per
    /// [`crate::KeyShape`]. The second field is the glob pattern
    /// (`None` = every key; `RANDOMKEY` carries none).
    Keyspace(crate::KeyShape, Option<Vec<u8>>),
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
