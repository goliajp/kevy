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
//! use kevy_rt::{Argv, Commands, Route, Runtime, Store, TxnKind};
//! use std::sync::Arc;
//! use std::sync::atomic::AtomicBool;
//!
//! #[derive(Clone)]
//! struct MyCommands;
//! impl Commands for MyCommands {
//!     fn route(&self, args: &Argv) -> Route {
//!         if args.len() >= 2 { Route::Single(1) } else { Route::Local }
//!     }
//!     fn dispatch(&self, _store: &mut Store, _args: &Argv) -> Vec<u8> {
//!         b"+OK\r\n".to_vec()
//!     }
//!     fn is_quit(&self, args: &Argv) -> bool {
//!         args.first().is_some_and(|c| c.eq_ignore_ascii_case(b"QUIT"))
//!     }
//!     fn is_write(&self, _args: &Argv) -> bool { false }
//!     fn txn_kind(&self, _args: &Argv) -> TxnKind { TxnKind::Other }
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
mod exec_op;
mod exec_pubsub;
mod inbox;
mod message;
mod reduce;
mod runtime;
mod shard;
#[cfg(target_os = "linux")]
mod uring_reactor;

pub use kevy_resp::Argv;
pub use kevy_store::Store;
pub use runtime::Runtime;

/// How a command maps onto shards.
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
    /// `PUBLISH channel message` — delivered to subscribers on every core.
    Publish,
}

/// Command-set semantics injected into the runtime. Cloned to every core, so it
/// must be cheap/stateless to clone.
pub trait Commands: Clone + Send + 'static {
    /// Classify how a command is routed across shards.
    fn route(&self, args: &Argv) -> Route;
    /// Execute a full command against one shard's store, returning RESP bytes.
    fn dispatch(&self, store: &mut Store, args: &Argv) -> Vec<u8>;
    /// Execute a command, appending the RESP reply to `out`. The in-order local
    /// fast path uses this to write straight into the connection's output buffer
    /// (no per-command reply `Vec`). Default: delegate to [`dispatch`](Self::dispatch).
    fn dispatch_into(&self, store: &mut Store, args: &Argv, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.dispatch(store, args));
    }
    /// Whether this command should close the connection (QUIT).
    fn is_quit(&self, args: &Argv) -> bool;
    /// Whether this command mutates the keyspace (so it must be logged to the AOF).
    fn is_write(&self, args: &Argv) -> bool;
    /// Transaction-control classification (MULTI/EXEC/DISCARD vs anything else).
    fn txn_kind(&self, args: &Argv) -> TxnKind;
    /// Called once per shard, immediately after [`Store::new`], before the
    /// reactor enters its event loop. Implementations install per-shard
    /// configuration that the runtime doesn't know about — currently the
    /// `maxmemory` + eviction-policy pair, which kevy ships via the
    /// process-wide [`crate::commands::config_global`] snapshot. Default:
    /// no-op so non-kevy embedders aren't forced to override.
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

    /// Resolve all verb-dependent attributes in **one** verb-table lookup.
    /// The default implementation calls the four per-attribute methods above
    /// (four upper_verb scans + matches); concrete impls SHOULD override this
    /// with a single match so the reactor's hot path pays the verb-resolution
    /// cost only once per command.
    fn resolve(&self, args: &Argv) -> ResolvedCmd {
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

/// Transaction-control classification for a command.
pub enum TxnKind {
    Multi,
    Exec,
    Discard,
    Other,
}
