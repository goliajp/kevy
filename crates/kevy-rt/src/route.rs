//! [`Route`] — how each command maps onto shards. Returned by
//! [`crate::Commands::route`] / carried in [`crate::ResolvedCmd`]; the
//! runtime's `start_command` matches on it to pick a dispatch shape.

use crate::exec_slowlog::SlowlogSub;

/// How a command maps onto shards.
#[derive(Debug)]
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
    /// Snapshot every shard's store to disk.
    Save,
    /// `BGREWRITEAOF` — rebuild every shard's AOF from in-memory state.
    /// Synchronous in v1.0 (each shard blocks for its own rewrite duration).
    RewriteAof,
    /// `MSET` — `args[1..]` are key/value pairs, routed per key's shard.
    MSet,
    /// `MGET` — `args[1..]` are keys; values gathered in request order.
    MGet,
    /// `SINTER` / `SUNION` / `SDIFF` — `args[1..]` are set keys.
    SInter,
    SUnion,
    SDiff,
    /// `KEYS pattern` — every shard returns its matching keys.
    Keys(Option<Vec<u8>>),
    /// `SCAN` (cursor-0 approximation) — like KEYS but replies `[cursor, keys]`.
    Scan(Option<Vec<u8>>),
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
    /// [`Commands::hello_reply`] hook so embedders set their own server
    /// metadata.
    Hello,
    /// `RENAME source destination` / `RENAMENX source destination`. The
    /// runtime handles the two-shard decision: same-shard renames go
    /// through one atomic [`Store::rename`] on the owning shard; cross-
    /// shard renames use the Take→Put orchestrator (lands in v2-3b;
    /// v2-3a emits `-CROSSSHARD ...` for that case).
    Rename {
        /// `true` for `RENAMENX` (no overwrite — reply `:0` if dst exists).
        nx: bool,
    },
    /// `SLOWLOG GET / LEN / RESET / HELP`. The sub-command + parsed
    /// args are pre-decoded at routing time so the runtime knows
    /// whether to short-circuit (HELP / error) or fan out across
    /// shards (GET / LEN / RESET). See [`parse_slowlog_sub`].
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
#[derive(Debug)]
pub struct XGroupCtx {
    /// Consumer-group name.
    pub group: Vec<u8>,
    /// Consumer name within the group.
    pub consumer: Vec<u8>,
    /// `NOACK` — deliver without adding to the PEL.
    pub noack: bool,
}
