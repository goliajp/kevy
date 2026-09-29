//! Public command-classification + live-config types for the [`Commands`]
//! trait (`ResolvedCmd`, `TxnKind`, `LiveRuntimeConfig`).
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
/// Built with [`ResolvedCmd::new`] and the `with_*` methods; every field
/// but the route has a default (an ordinary, non-blocking, read-only
/// command outside a transaction).
///
/// ```
/// use kevy_rt::{ResolvedCmd, Route, TxnKind};
///
/// let set = ResolvedCmd::new(Route::Single(1)).with_write(true);
/// assert!(set.is_write && !set.is_quit);
/// assert_eq!(set.txn_kind, TxnKind::Other);
/// ```
///
/// [`Commands::resolve`]: crate::Commands::resolve
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ResolvedCmd {
    /// MULTI/EXEC/DISCARD/WATCH classification, so the transaction layer
    /// does not re-parse the verb.
    pub txn_kind: TxnKind,
    /// Where this command goes: one shard, all of them, or a local answer.
    pub route: Route,
    /// `QUIT`, which the reactor answers and then closes on rather than
    /// dispatching.
    pub is_quit: bool,
    /// Whether the command mutates — the AOF and replication gate. Set
    /// from the verb table, not inferred from the route.
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

impl ResolvedCmd {
    /// A command routed by `route`, with every other attribute at its
    /// default: [`TxnKind::Other`], not QUIT, not a write, not blocking,
    /// wakes no waiter.
    ///
    /// ```
    /// let ping = kevy_rt::ResolvedCmd::new(kevy_rt::Route::Local);
    /// assert!(!ping.is_write && ping.wake_idx.is_none());
    /// ```
    #[inline]
    #[must_use]
    pub fn new(route: Route) -> Self {
        ResolvedCmd {
            txn_kind: TxnKind::Other,
            route,
            is_quit: false,
            is_write: false,
            block_hint: BlockHint::None,
            wake_idx: None,
        }
    }

    /// Set [`Self::txn_kind`].
    ///
    /// ```
    /// use kevy_rt::{ResolvedCmd, Route, TxnKind};
    /// let multi = ResolvedCmd::new(Route::Local).with_txn_kind(TxnKind::Multi);
    /// assert_eq!(multi.txn_kind, TxnKind::Multi);
    /// ```
    #[inline]
    #[must_use]
    pub fn with_txn_kind(mut self, txn_kind: TxnKind) -> Self {
        self.txn_kind = txn_kind;
        self
    }

    /// Set [`Self::is_quit`].
    ///
    /// ```
    /// let quit = kevy_rt::ResolvedCmd::new(kevy_rt::Route::Local).with_quit(true);
    /// assert!(quit.is_quit);
    /// ```
    #[inline]
    #[must_use]
    pub fn with_quit(mut self, is_quit: bool) -> Self {
        self.is_quit = is_quit;
        self
    }

    /// Set [`Self::is_write`].
    ///
    /// ```
    /// let del = kevy_rt::ResolvedCmd::new(kevy_rt::Route::DelKeys).with_write(true);
    /// assert!(del.is_write);
    /// ```
    #[inline]
    #[must_use]
    pub fn with_write(mut self, is_write: bool) -> Self {
        self.is_write = is_write;
        self
    }

    /// Set [`Self::block_hint`].
    ///
    /// ```
    /// use kevy_rt::{BlockHint, BlockKind, ResolvedCmd, Route};
    ///
    /// let hint = BlockHint::Block { kind: BlockKind::Blpop, keys: vec![b"q".to_vec()], timeout_ms: 0 };
    /// let blpop = ResolvedCmd::new(Route::Single(1)).with_block_hint(hint.clone());
    /// assert_eq!(blpop.block_hint, hint);
    /// ```
    #[inline]
    #[must_use]
    pub fn with_block_hint(mut self, block_hint: BlockHint) -> Self {
        self.block_hint = block_hint;
        self
    }

    /// Set [`Self::wake_idx`].
    ///
    /// ```
    /// let lpush = kevy_rt::ResolvedCmd::new(kevy_rt::Route::Single(1)).with_wake_idx(Some(1));
    /// assert_eq!(lpush.wake_idx, Some(1));
    /// ```
    #[inline]
    #[must_use]
    pub fn with_wake_idx(mut self, wake_idx: Option<u8>) -> Self {
        self.wake_idx = wake_idx;
        self
    }
}

/// Outcome of an extension fan-out reduce ([`Commands::extension_reduce`]).
///
/// ```
/// let done = kevy_rt::ExtensionReduced::Reply(b"+OK\r\n".to_vec());
/// assert!(matches!(done, kevy_rt::ExtensionReduced::Reply(_)));
/// ```
///
/// [`Commands::extension_reduce`]: crate::Commands::extension_reduce
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ExtensionReduced {
    /// The final RESP reply bytes for the client.
    Reply(Vec<u8>),
    /// Not final yet: fan `argv` out as a follow-up extension phase —
    /// to the shards [`Commands::extension_targets`] names for it, every
    /// shard by default — and reduce again when its chunks land. Phase
    /// state rides inside the argv itself, so the runtime holds no
    /// per-phase bookkeeping.
    ///
    /// [`Commands::extension_targets`]: crate::Commands::extension_targets
    Continue(Vec<Vec<u8>>),
}

/// Transaction-control classification for a command.
///
/// ```
/// assert_eq!(kevy_rt::TxnKind::default(), kevy_rt::TxnKind::Other);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum TxnKind {
    /// `MULTI` — opens a queue on this connection.
    Multi,
    /// `EXEC` — runs the queue, or replies nil if a WATCH was broken.
    Exec,
    /// `DISCARD` — drops the queue and any WATCH set.
    Discard,
    /// `WATCH` — outside MULTI runs the fan-out; inside MULTI is rejected
    /// with an error (Redis semantics: `WATCH inside MULTI is not allowed`).
    /// `UNWATCH` is plain [`Self::Other`] — outside MULTI it routes to
    /// [`Route::Unwatch`] (clear + OK); inside MULTI it queues as a no-op
    /// that dispatch resolves to +OK at EXEC time.
    Watch,
    /// Everything else: queued inside MULTI, dispatched outside it.
    #[default]
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
/// ```
/// let mut live = kevy_rt::LiveRuntimeConfig::default();
/// live.tick_interval_ms = Some(50);
/// assert!(live.appendfsync.is_none());
/// ```
///
/// [`Commands`]: crate::Commands
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct LiveRuntimeConfig {
    /// AOF fsync policy. Applied via `Aof::set_fsync` — switching to
    /// `Always` mid-flight also flushes any buffered bytes so the new
    /// "every write is on disk before reply" contract is honoured from
    /// the next append onward.
    pub appendfsync: Option<Fsync>,
    /// `auto_aof_rewrite_percentage`. `0` disables the auto-trigger.
    pub auto_aof_rewrite_pct: Option<u32>,
    /// Absolute-size auto-rewrite trigger in bytes (0 = rule off).
    pub auto_aof_rewrite_bytes: Option<u64>,
    /// Time-based auto-rewrite trigger in seconds (0 = rule off).
    pub auto_aof_rewrite_interval_secs: Option<u64>,
    /// `auto_aof_rewrite_min_size` in bytes.
    pub auto_aof_rewrite_min_size: Option<u64>,
    /// New tick interval in ms (`1000/hz`). `0` disables ticking
    /// entirely — note that disabling also turns off active TTL
    /// expiry and the auto-rewrite tick path. Lazy expiry on access
    /// always still works.
    pub tick_interval_ms: Option<u64>,
    /// `notify_keyspace_events` flags. Parsed by the [`Commands`]
    /// impl from its config source (e.g. kevy reads
    /// `config_global` and parses the flag string into
    /// [`NotificationFlags`]). Flags with no channel or no event class
    /// mean OFF — writes pay one mask test and skip every per-key
    /// keyspace notification publish.
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
    /// Monotonic promotion counter. The command layer bumps
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
/// ```
/// let ack = kevy_rt::ReplicaAck::new(42, 15);
/// assert_eq!((ack.acked_offset, ack.ack_age_ms), (42, 15));
/// ```
///
/// [`Commands::on_replication_view`]: crate::Commands::on_replication_view
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub struct ReplicaAck {
    /// Offset from the latest `REPLCONF ACK` (`0` is a real heartbeat
    /// ACK from an empty replica, not a placeholder).
    pub acked_offset: u64,
    /// Milliseconds since that ACK was received, measured when the
    /// view was published. Feeds the `min_replicas_max_lag_ms` gate.
    pub ack_age_ms: u64,
}

impl ReplicaAck {
    /// An ACK of `acked_offset`, received `ack_age_ms` milliseconds before
    /// the view was published.
    ///
    /// ```
    /// assert_eq!(kevy_rt::ReplicaAck::new(0, 0), kevy_rt::ReplicaAck::default());
    /// ```
    #[must_use]
    pub const fn new(acked_offset: u64, ack_age_ms: u64) -> Self {
        Self { acked_offset, ack_age_ms }
    }
}

/// One replica conn's row in the per-tick replication view:
/// `(replica_id, peer_ipv4, peer_port, sent_offset, ack)`. The id is
/// the identity string the replica presented at handshake — command
/// layers group per-shard rows by it to render one aggregate entry
/// per replica process.
pub type ReplicaViewRow = (String, std::net::Ipv4Addr, u16, u64, Option<ReplicaAck>);
