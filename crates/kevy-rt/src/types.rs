//! Public command-classification + live-config types for the [`Commands`]
//! trait (`ResolvedCmd`, `NotifyClass`, `TxnKind`, `LiveRuntimeConfig`).
//! Split out of `lib.rs` (500-LOC house rule); all re-exported from the
//! crate root, so the public paths (`kevy_rt::TxnKind`, …) are unchanged.
//!
//! [`Commands`]: crate::Commands

use crate::blocked::BlockHint;
use crate::route::Route;
use kevy_config::NotificationFlags;
use kevy_persist::Fsync;

/// Per-command verb-resolution result. Produced once by [`Commands::resolve`]
/// in the reactor's parse-then-dispatch loop, reused for routing decisions,
/// AOF logging, and the QUIT branch — so the per-cmd `upper_verb` cost goes
/// from 4× down to 1×.
///
/// [`Commands::resolve`]: crate::Commands::resolve
pub struct ResolvedCmd {
    pub txn_kind: TxnKind,
    pub route: Route,
    pub is_quit: bool,
    pub is_write: bool,
    /// Blocking-command classification (see [`Commands::block_hint`]).
    /// `BlockHint::None` for every non-blocking verb.
    ///
    /// [`Commands::block_hint`]: crate::Commands::block_hint
    pub block_hint: BlockHint,
    /// Index into `args` whose write may wake a `BLPOP` / `XREAD BLOCK`
    /// waiter parked on that key — `Some(1)` for `LPUSH` / `RPUSH` /
    /// `XADD`, `None` for every other command (including reads). The
    /// dispatcher's wake hook is gated on both this being `Some` *and*
    /// the per-shard `BlockedClients` registry being non-empty, so the
    /// steady-state cost when nobody is parked is one `is_empty()` check.
    pub wake_idx: Option<u8>,
}

/// Keyspace-notification event class — what category a write command
/// belongs to, so the runtime can match it against the per-conn
/// notify_keyspace_events flags before publishing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyClass {
    /// `g` — generic key commands (DEL / EXPIRE / PERSIST / RENAME / TYPE).
    Generic,
    /// `$` — string commands (SET / GETSET / INCR / APPEND / MSET).
    String,
    /// `l` — list commands (LPUSH / RPUSH / LPOP / LREM / LTRIM / …).
    List,
    /// `s` — set commands (SADD / SREM / SPOP / …).
    Set,
    /// `h` — hash commands (HSET / HDEL / HINCRBY / …).
    Hash,
    /// `z` — sorted-set commands (ZADD / ZREM / ZINCRBY / …).
    Zset,
    /// `t` — stream commands (XADD / XDEL / XTRIM / XGROUP / XACK /
    /// XCLAIM / XREADGROUP / …). Matches Redis's `t` class.
    Stream,
}

impl NotifyClass {
    /// Whether `flags` enables this event class.
    #[inline]
    pub fn enabled_in(self, flags: &NotificationFlags) -> bool {
        match self {
            NotifyClass::Generic => flags.generic,
            NotifyClass::String => flags.string,
            NotifyClass::List => flags.list,
            NotifyClass::Set => flags.set,
            NotifyClass::Hash => flags.hash,
            NotifyClass::Zset => flags.zset,
            NotifyClass::Stream => flags.stream,
        }
    }
}

/// Outcome of an extension fan-out reduce ([`Commands::extension_reduce`]).
///
/// [`Commands::extension_reduce`]: crate::Commands::extension_reduce
#[derive(Debug, PartialEq, Eq)]
pub enum ExtensionReduced {
    /// The final RESP reply bytes for the client.
    Reply(Vec<u8>),
    /// Not final yet: fan `argv` out to every shard as a follow-up
    /// extension phase and reduce again when its chunks land. Phase
    /// state rides inside the argv itself, so the runtime holds no
    /// per-phase bookkeeping.
    Continue(Vec<Vec<u8>>),
}

/// Transaction-control classification for a command.
pub enum TxnKind {
    Multi,
    Exec,
    Discard,
    /// `WATCH` — outside MULTI runs the fan-out; inside MULTI is rejected
    /// with an error (Redis semantics: `WATCH inside MULTI is not allowed`).
    /// `UNWATCH` is plain [`Self::Other`] — outside MULTI it routes to
    /// [`Route::Unwatch`] (clear + OK); inside MULTI it queues as a no-op
    /// that dispatch resolves to +OK at EXEC time.
    Watch,
    Other,
}

/// Live snapshot of the runtime-owned knobs that may have been changed
/// since this shard's last tick. Built by the [`Commands`] impl from
/// its own config source (e.g. kevy reads `config_global`). Each
/// `Some(_)` is applied to the shard; each `None` leaves the existing
/// setting alone.
///
/// One snapshot is built per tick (every 100 ms by default), so its
/// cost is amortised across thousands of commands.
///
/// [`Commands`]: crate::Commands
#[derive(Debug, Default, Clone, Copy)]
pub struct LiveRuntimeConfig {
    /// AOF fsync policy. Applied via `Aof::set_fsync` — switching to
    /// `Always` mid-flight also flushes any buffered bytes so the new
    /// "every write is on disk before reply" contract is honoured from
    /// the next append onward.
    pub appendfsync: Option<Fsync>,
    /// `auto_aof_rewrite_percentage`. `0` disables the auto-trigger.
    pub auto_aof_rewrite_pct: Option<u32>,
    /// `auto_aof_rewrite_min_size` in bytes.
    pub auto_aof_rewrite_min_size: Option<u64>,
    /// New tick interval in ms (`1000/hz`). `0` disables ticking
    /// entirely — note that disabling also turns off active TTL
    /// expiry and the auto-rewrite tick path. Lazy expiry on access
    /// always still works.
    pub tick_interval_ms: Option<u64>,
    /// `notify_keyspace_events` flags. Parsed by the [`Commands`]
    /// impl from its config source (e.g. kevy reads
    /// `config_global` + [`kevy_config::parse_notification_flags`]).
    /// Default-empty flags mean OFF — writes pay one bool-OR check
    /// and skip every per-key keyspace notification publish.
    ///
    /// [`Commands`]: crate::Commands
    pub notify_flags: Option<NotificationFlags>,
    /// `[slowlog].slower_than_micros` — `-1` disables, `0` records all,
    /// `>0` is the strict micros threshold. `None` keeps the existing
    /// shard setting (set by the [`Runtime`] builder at startup).
    ///
    /// [`Runtime`]: crate::Runtime
    pub slowlog_slower_than_micros: Option<i64>,
    /// `[slowlog].max_len` — ring cap per shard. Shrinking trims the
    /// oldest entries on the next tick application.
    pub slowlog_max_len: Option<u32>,
    /// v3.16 D2 — monotonic promotion counter. The command layer bumps
    /// it every time this process is PROMOTED (replica → primary:
    /// `REPLICAOF NO ONE` on a following replica, or an election win).
    /// Each shard tracks the last value it saw; an increase makes the
    /// shard bump its feed generation (offsets restart at 0, persisted
    /// via the feed-gen sidecar) — so a REPL.TOKEN minted before the
    /// failover can never falsely satisfy a REPL.WAIT against the new
    /// primary's unrelated offset space. Not an Option: `0` (the
    /// default) means "never promoted" and embedders pay nothing.
    pub promotion_epoch: u64,
}

/// A replica's acknowledged state, published per shard tick via
/// [`Commands::on_replication_view`]: the offset from its latest
/// `REPLCONF ACK` plus that ACK's age at publication time. `None` in
/// the view tuple means the replica has never ACKed.
///
/// [`Commands::on_replication_view`]: crate::Commands::on_replication_view
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplicaAck {
    /// Offset from the latest `REPLCONF ACK` (`0` is a real heartbeat
    /// ACK from an empty replica, not a placeholder).
    pub acked_offset: u64,
    /// Milliseconds since that ACK was received, measured when the
    /// view was published. Feeds the `min_replicas_max_lag_ms` gate.
    pub ack_age_ms: u64,
}
