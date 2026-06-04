//! kevy-rt — shared-nothing, thread-per-core runtime.
//!
//! Each core runs its own reactor (kqueue/epoll) and owns one **shard** of the
//! keyspace (`hash(key) % nshards`). There is no shared mutable state and no
//! lock on the hot path — cores communicate only by message passing over
//! channels, woken via a self-pipe ([`kevy_sys::Waker`]). Connections are spread
//! across cores by `SO_REUSEPORT`; a command whose key lives on another core is
//! forwarded to that core, executed there, and the reply routed back to the
//! originating connection.
//!
//! Per-connection reply ordering is preserved (RESP is pipelined): each command
//! gets a monotonic seq; replies are emitted only in contiguous seq order, so an
//! async cross-core reply never overtakes an earlier one.
//!
//! The cross-core channel currently uses `std::sync::mpsc` (pure Rust, zero
//! deps); swapping in a lock-free SPSC/MPSC ring is a perf-polish item.
//! Command semantics are injected via the [`Commands`] trait, keeping the
//! runtime independent of the concrete command set. Part of the [kevy] server.
//!
//! [kevy]: https://crates.io/crates/kevy
//!
//! # Module map
//!
//! - [`Runtime`] (in `runtime`) — public entry point; spawns one `shard` per core.
//! - `shard` — the per-core reactor: sockets, the inbound queue, reply flushing.
//! - `exec` — command semantics: routing, execution, and result reduction.
//! - `message` — internal cross-core work/result types.
//! - `conn` — per-connection state (input/output, seq ring, subscriptions).
//! - `reduce` — reply reduction (`materialize`) and pure helpers (set algebra,
//!   shard hashing, pub/sub framing).
//!
//! # Example
//!
//! Implement [`Commands`] for your command set and run it. ([`Store`] is
//! re-exported so you don't need a separate dependency.)
//!
//! ```no_run
//! use kevy_rt::{ArgvView, Commands, Route, Runtime, Store, TxnKind};
//! use std::sync::Arc;
//! use std::sync::atomic::AtomicBool;
//!
//! #[derive(Clone)]
//! struct MyCommands;
//! impl Commands for MyCommands {
//!     fn route<A: ArgvView + ?Sized>(&self, args: &A) -> Route {
//!         if args.len() >= 2 { Route::Single(1) } else { Route::Local }
//!     }
//!     fn dispatch<A: ArgvView + ?Sized>(&self, _store: &mut Store, _args: &A) -> Vec<u8> {
//!         b"+OK\r\n".to_vec()
//!     }
//!     fn is_quit<A: ArgvView + ?Sized>(&self, args: &A) -> bool {
//!         args.first().is_some_and(|c| c.eq_ignore_ascii_case(b"QUIT"))
//!     }
//!     fn is_write<A: ArgvView + ?Sized>(&self, _args: &A) -> bool { false }
//!     fn txn_kind<A: ArgvView + ?Sized>(&self, _args: &A) -> TxnKind { TxnKind::Other }
//! }
//!
//! // One shard per core, listening on 127.0.0.1:6379, until `stop` is set.
//! let rt = Runtime::new([127, 0, 0, 1], 6379, 4, MyCommands);
//! rt.run(Arc::new(AtomicBool::new(false))).unwrap();
//! ```
// Almost entirely safe: the only `unsafe` is in `uring_reactor` (Linux io_uring),
// which needs raw buffer pointers for zero-allocation completion I/O — on the hot
// path toward kevy's disk-I/O-ceiling goal, where a buffer-ownership safe wrapper
// would add per-op cost. Each such block documents its invariant; the
// epoll/kqueue path and every other module stay safe, and all libc lives in
// kevy-sys.
#![deny(unsafe_op_in_unsafe_fn)]

mod conn;
mod exec;
mod exec_build;
mod exec_notify;
mod exec_op;
mod exec_pubsub;
mod exec_pubsub_pattern;
mod exec_watch;
mod inbox;
mod message;
mod reduce;
mod runtime;
mod shard;
#[cfg(target_os = "linux")]
mod uring_reactor;

pub use kevy_config::NotificationFlags;
pub use kevy_persist::Fsync;
pub use kevy_resp::{Argv, ArgvBorrowed, ArgvView, RespVersion};
pub use kevy_store::Store;
pub use runtime::Runtime;

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
}

/// Command-set semantics injected into the runtime. Cloned to every core, so it
/// must be cheap/stateless to clone.
pub trait Commands: Clone + Send + 'static {
    /// Classify how a command is routed across shards.
    fn route<A: ArgvView + ?Sized>(&self, args: &A) -> Route;
    /// Execute a full command against one shard's store, returning RESP bytes.
    fn dispatch<A: ArgvView + ?Sized>(&self, store: &mut Store, args: &A) -> Vec<u8>;
    /// RESP3 variant of [`Self::dispatch`] — called when the connection
    /// has negotiated `HELLO 3`. Default: delegate to the RESP2 path
    /// (the cross-shard `Op::Dispatch` carries a per-cmd `RespVersion`
    /// so a V2 client and a V3 client can share the owning shard).
    fn dispatch_resp3<A: ArgvView + ?Sized>(&self, store: &mut Store, args: &A) -> Vec<u8> {
        self.dispatch(store, args)
    }
    /// Execute a command, appending the RESP reply to `out`. The in-order local
    /// fast path uses this to write straight into the connection's output buffer
    /// (no per-command reply `Vec`). Default: delegate to [`dispatch`](Self::dispatch).
    fn dispatch_into<A: ArgvView + ?Sized>(&self, store: &mut Store, args: &A, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.dispatch(store, args));
    }
    /// RESP3 variant of [`Self::dispatch_into`] — called when the
    /// connection has negotiated `HELLO 3`. Default: delegate to the
    /// RESP2 path (so a server that hasn't migrated any replies still
    /// works correctly with a RESP3 client, per spec). Override per
    /// command to emit RESP3 shapes (Map / Set / Double / …).
    fn dispatch_into_resp3<A: ArgvView + ?Sized>(
        &self,
        store: &mut Store,
        args: &A,
        out: &mut Vec<u8>,
    ) {
        self.dispatch_into(store, args, out);
    }
    /// Classify a command for keyspace notifications. Returns `Some`
    /// for write commands that should fire a notification when the
    /// corresponding flag is enabled; `None` for read-only / no-op /
    /// not-yet-classified commands (those never publish). Default
    /// `None` so non-kevy embedders pay nothing.
    fn notify_class<A: ArgvView + ?Sized>(&self, _args: &A) -> Option<NotifyClass> {
        None
    }

    /// Handle `HELLO` — return the new connection protocol version + the
    /// reply bytes. The runtime applies the new version to the conn
    /// before scheduling the reply, so a `HELLO 3` ack itself comes out
    /// shaped as a RESP3 Map (the new protocol is in effect for its own
    /// reply).
    ///
    /// Default: ignore the args, keep `current_proto`, emit a minimal
    /// RESP2 +OK so embedders that don't care still see a sane reply.
    /// kevy's own impl in `kevy::KevyCommands` parses the optional
    /// protover and emits the full server-info shape.
    fn hello_reply<A: ArgvView + ?Sized>(
        &self,
        _args: &A,
        current_proto: RespVersion,
    ) -> (RespVersion, Vec<u8>) {
        (current_proto, b"+OK\r\n".to_vec())
    }
    /// Whether this command should close the connection (QUIT).
    fn is_quit<A: ArgvView + ?Sized>(&self, args: &A) -> bool;
    /// Whether this command mutates the keyspace (so it must be logged to the AOF).
    fn is_write<A: ArgvView + ?Sized>(&self, args: &A) -> bool;
    /// Transaction-control classification (MULTI/EXEC/DISCARD vs anything else).
    fn txn_kind<A: ArgvView + ?Sized>(&self, args: &A) -> TxnKind;
    /// Called once per shard, immediately after [`Store::new`], before the
    /// reactor enters its event loop. Implementations install per-shard
    /// configuration that the runtime doesn't know about — currently the
    /// `maxmemory` + eviction-policy pair, which kevy ships via its own
    /// process-wide config snapshot. Default: no-op so non-kevy embedders
    /// aren't forced to override.
    fn on_shard_init(&self, _store: &mut Store) {}

    /// Periodic shard housekeeping (the equivalent of Redis's `serverCron`).
    /// kevy uses this to run [`Store::tick_expire`] at the configured
    /// `[expiry].hz`. Default no-op so non-kevy embedders / runtimes can
    /// ignore it.
    fn on_shard_tick(&self, _store: &mut Store) {}

    /// Interval between [`Self::on_shard_tick`] calls. Default 100 ms
    /// (matching Redis's `hz = 10`). `0` disables ticking entirely.
    fn shard_tick_interval_ms(&self) -> u64 {
        100
    }

    /// Snapshot of the runtime-owned knobs that can be hot-modified
    /// (the kevy server wires this to `CONFIG SET`). Called once per
    /// shard tick — each `Some` value is applied to the shard's live
    /// state; each `None` keeps the existing setting untouched.
    ///
    /// Default returns all-None so embedders that never hot-swap config
    /// pay nothing beyond one struct-build per tick. The cost lives in
    /// the impl's read of its own config source.
    fn live_runtime_config(&self) -> LiveRuntimeConfig {
        LiveRuntimeConfig::default()
    }

    /// Resolve all verb-dependent attributes in **one** verb-table lookup.
    /// The default implementation calls the four per-attribute methods above
    /// (four upper_verb scans + matches); concrete impls SHOULD override this
    /// with a single match so the reactor's hot path pays the verb-resolution
    /// cost only once per command.
    fn resolve<A: ArgvView + ?Sized>(&self, args: &A) -> ResolvedCmd {
        ResolvedCmd {
            txn_kind: self.txn_kind(args),
            route: self.route(args),
            is_quit: self.is_quit(args),
            is_write: self.is_write(args),
        }
    }
}

/// Per-command verb-resolution result. Produced once by [`Commands::resolve`]
/// in the reactor's parse-then-dispatch loop, reused for routing decisions,
/// AOF logging, and the QUIT branch — so the per-cmd `upper_verb` cost goes
/// from 4× down to 1×.
pub struct ResolvedCmd {
    pub txn_kind: TxnKind,
    pub route: Route,
    pub is_quit: bool,
    pub is_write: bool,
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
        }
    }
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
    pub notify_flags: Option<NotificationFlags>,
}
