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
//! let rt = Runtime::builder(MyCommands).bind([127, 0, 0, 1], 6379).shards(4);
//! rt.run(Arc::new(AtomicBool::new(false))).unwrap();
//! ```
// Almost entirely safe: the only `unsafe` is in `uring_reactor` (Linux io_uring),
// which needs raw buffer pointers for zero-allocation completion I/O — on the hot
// path toward kevy's disk-I/O-ceiling goal, where a buffer-ownership safe wrapper
// would add per-op cost. Each such block documents its invariant; the
// epoll/kqueue path and every other module stay safe, and all libc lives in
// kevy-sys.
#![deny(unsafe_op_in_unsafe_fn)]

mod bio;
mod block_xshard;
mod blocked;
mod lua_wake_bridge;
mod cache_padded;
mod cluster;
mod conn;
mod exec;
mod exec_build;
mod exec_client_intercept;
mod exec_crossslot;
mod exec_dispatch;
mod exec_fold;
mod exec_notify;
mod exec_op;
mod exec_pubsub;
mod exec_pubsub_pattern;
mod exec_rename;
mod exec_replwait;
mod exec_feed;
mod exec_zalgebra;
mod exec_slowlog;
mod exec_watch;
mod inbox;
mod persist_worker;
mod message;
mod message_agg;
mod reduce;
mod replica_inbox;
mod replication;
mod replication_apply;
mod replication_gate;
mod replication_io;
mod replication_pump;
mod reshard;
mod route;
mod runtime;
mod runtime_builders;
mod runtime_run;
mod shard;
mod shard_flush;
mod shard_lifecycle;
mod shard_run;
mod shard_tick;
mod types;
#[cfg(target_os = "linux")]
mod uring_arm;
#[cfg(target_os = "linux")]
mod uring_bigbulk;
#[cfg(target_os = "linux")]
mod uring_bigbulk_b2alt;
#[cfg(target_os = "linux")]
mod uring_bigbulk_probe;
#[cfg(target_os = "linux")]
mod uring_conn;
#[cfg(target_os = "linux")]
mod uring_inbox;
#[cfg(target_os = "linux")]
mod uring_io;
#[cfg(target_os = "linux")]
mod uring_ops;
#[cfg(target_os = "linux")]
mod uring_park;
#[cfg(target_os = "linux")]
mod uring_reactor;
#[cfg(target_os = "linux")]
mod uring_setup;

pub use blocked::{BlockHint, BlockKind};
pub use lua_wake_bridge::push_lua_wake_key;
pub use reduce::shard_of as shard_of_key;
pub use cluster::shard_slot_range;
pub use exec_slowlog::{SlowlogSub, parse_slowlog_sub};
pub use kevy_config::NotificationFlags;
pub use kevy_persist::Fsync;
pub use kevy_resp::{Argv, ArgvBorrowed, ArgvView, RespVersion};
pub use kevy_store::Store;
pub use replica_inbox::{ReplicaApply, ReplicaInboxReceiver, ReplicaInboxSender, replica_inbox_pair};
pub use replication_gate::ReplicatedApplyGuard;
pub use route::{Route, XGroupCtx};
pub use message::{KeyShape, MultiOp, ZCombine};
pub use runtime::Runtime;
pub use types::{
    ExtensionReduced, LiveRuntimeConfig, NotifyClass, ReplicaAck, ResolvedCmd, TxnKind,
};

/// Command-set semantics injected into the runtime. Cloned to every core, so it
/// must be cheap/stateless to clone.
pub trait Commands: Clone + Send + 'static {
    /// Classify how a command is routed across shards.
    fn route<A: ArgvView + ?Sized>(&self, args: &A) -> Route;
    /// Execute a full command against one shard's store, returning RESP bytes.
    fn dispatch<A: ArgvView + ?Sized>(&self, store: &mut Store, args: &A) -> Vec<u8>;
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

    /// Called once on the shard's own thread, first thing in the reactor
    /// entry (both reactors), before restore/replay. Implementations that
    /// need per-shard identity at dispatch time (e.g. kevy's `CLUSTER MYID`
    /// / `CLUSTER NODES` `myself` flag) stash `shard` in a thread-local here
    /// — in a thread-per-core runtime the current thread *is* the shard.
    /// Default: no-op.
    fn on_shard_start(&self, _shard: usize) {}

    /// Per-tick persistence-stats publication: whether this shard has a
    /// background save/rewrite in flight and how many AOF rewrites have
    /// completed since open. Command layers that serve `INFO persistence`
    /// stash these in a thread-local (thread-per-core: the answering
    /// thread *is* the shard, same pattern as [`Self::on_shard_start`]).
    /// Default: no-op.
    fn on_persist_stats(&self, _in_flight: bool, _aof_rewrites_total: u64) {}

    /// Per-tick replication-view publication: the answering shard's
    /// current `master_repl_offset` (== `ReplicationSource::next_offset()`)
    /// plus a `(ipv4, port, sent_offset, ack)` entry for every
    /// handshake-complete replica (in `AckSent`, `Streaming`, or
    /// `SnapshotShipping`); `ack` is `None` until the replica's first
    /// `REPLCONF ACK`. `connected_slaves` for `INFO` / `ROLE` is
    /// derived as `replicas.len()`.
    /// Only called when this shard has a `ReplicationSource`
    /// installed (i.e. `Runtime::with_replication(true, ...)` was
    /// requested); standalone setups pay nothing. Command layers
    /// that serve `ROLE` / `INFO replication` stash the values in a
    /// thread-local (thread-per-core: the answering thread *is* the
    /// shard, same pattern as [`Self::on_persist_stats`]). Default
    /// no-op.
    fn on_replication_view(
        &self,
        _master_repl_offset: u64,
        _replicas: Vec<(std::net::Ipv4Addr, u16, u64, Option<ReplicaAck>)>,
    ) {}

    /// Periodic shard housekeeping (the equivalent of Redis's `serverCron`).
    /// kevy uses this to run [`Store::tick_expire`] at the configured
    /// `[expiry].hz`. Default no-op so non-kevy embedders / runtimes can
    /// ignore it.
    fn on_shard_tick(&self, _store: &mut Store) {}

    /// Per-shard half of an extension fan-out command (IDX.* /
    /// future VIEW.* / FT.*): compute this shard's raw chunk for
    /// `argv`. The payload encoding is the embedder's own — the
    /// runtime treats it as opaque bytes and hands all chunks to
    /// [`Commands::extension_reduce`] at the origin.
    fn extension_op(&self, _store: &mut Store, _argv: &[Vec<u8>]) -> Vec<u8> {
        Vec::new()
    }

    /// Pre-dispatch write gate. `Some(err_bytes)` rejects every
    /// data-write client command with that RESP error before any
    /// routing (replication apply does NOT pass through here, so a
    /// read-only replica keeps applying its feed). Admin verbs
    /// (REPLICAOF / CONFIG) are not classified as writes and stay
    /// available as the operator escape hatch. Default: writes always
    /// allowed.
    fn write_denied(&self) -> Option<Vec<u8>> {
        None
    }

    /// Bounded staleness: called before READ verbs; return
    /// `Some(error_bytes)` to refuse the read (a replica whose feed
    /// is staler than the configured bound answers `-STALE` so the
    /// client falls back to the primary). Default: reads always
    /// allowed.
    fn read_denied(&self) -> Option<Vec<u8>> {
        None
    }

    /// Origin-side reduce of an extension fan-out — merge every
    /// shard's chunk (produced by [`Self::extension_op`]) into either
    /// the final RESP reply or a follow-up fan-out argv (see
    /// [`ExtensionReduced`]). `proto` is the requesting connection's
    /// negotiated RESP version so proto-aware reduces can shape the
    /// reply (Map vs pair-array).
    fn extension_reduce(
        &self,
        _argv: &[Vec<u8>],
        _chunks: Vec<Vec<u8>>,
        _proto: kevy_resp::RespVersion,
    ) -> ExtensionReduced {
        ExtensionReduced::Reply(b"-ERR extension commands not supported\r\n".to_vec())
    }

    /// Called after every applied write with the written key
    /// (when the resolver knew one). Default no-op; kevy uses it for
    /// synchronous secondary-index maintenance (derived-by-
    /// construction). Runs on the shard thread with store access —
    /// implementations must be cheap when their feature is off.
    fn on_write(&self, _store: &mut Store, _key: &[u8]) {}

    /// Called once per client command at dispatch entry (before routing /
    /// fan-out, so a multi-key command counts once). kevy uses it for
    /// `INFO stats: total_commands_processed`. Hot path — keep it to a single
    /// thread-local bump. Default no-op so non-kevy embedders pay nothing.
    fn on_command(&self) {}

    /// Called once per accepted client connection. kevy uses it for
    /// `INFO stats: total_connections_received`. Default no-op.
    fn on_connection(&self) {}

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

    /// Classify a command for blocking semantics. `BlockHint::None`
    /// (default) is the zero-cost answer for every non-blocking verb;
    /// the dispatcher only registers a waiter when this returns
    /// `BlockHint::Block` *and* the command's `dispatch_into` produced no
    /// reply (i.e. it could not satisfy itself immediately — e.g. BLPOP
    /// on an empty list). Concrete impls should fold this into their
    /// override of [`Self::resolve`] so the verb-table lookup happens
    /// once per command.
    fn block_hint<A: ArgvView + ?Sized>(&self, _args: &A) -> BlockHint {
        BlockHint::None
    }

    /// Rewrite `args` into the owned [`Argv`] that the dispatcher will
    /// store as the parked waiter's command and replay on wake. Lets a
    /// command set normalise positional ID / cursor arguments that would
    /// otherwise re-resolve to a different value on retry — most notably
    /// `XREAD BLOCK ... STREAMS k $`, where leaving `$` literal in the
    /// retried argv causes a fresh re-resolve to the post-`XADD` last_id
    /// and zero matching entries (the wake hangs).
    ///
    /// Default: just materialise the argv unchanged. Concrete impls only
    /// need to override when a registered command carries an arg whose
    /// meaning depends on store state at park time (`XREAD $`, the
    /// classic case).
    ///
    /// For the cross-shard arbiter this runs on the **target** shard (the
    /// one that owns the key) when the waiter is armed, so `$` snapshots
    /// the target's real `last_id` — not the origin shard's (which may not
    /// hold the stream at all).
    fn resolve_block_argv<A: ArgvView + ?Sized>(
        &self,
        _store: &mut Store,
        args: &A,
        _kind: BlockKind,
    ) -> Argv {
        args.to_argv()
    }

    /// Build the **single-key** command the dispatcher will replay to
    /// satisfy one watched `key` of a (possibly multi-key) blocking
    /// command. `args` is the original command; `key` is one of its
    /// watched keys. Returns an [`Argv`] that, when dispatched, pops /
    /// reads only `key` — e.g. `BLPOP k1 k2 0` watching `k2` yields
    /// `BLPOP k2 0`; `XREAD … STREAMS s1 s2 id1 id2` watching `s2`
    /// yields `XREAD … STREAMS s2 id2`.
    ///
    /// Any state-dependent positional arg (`$`) is left **literal** here —
    /// it's frozen later by [`Self::resolve_block_argv`] on the key's
    /// owning shard. No store access needed (pure argv slicing). Default:
    /// the unchanged argv (single-key blocking commands need no rewrite).
    fn block_serve_argv<A: ArgvView + ?Sized>(
        &self,
        args: &A,
        _kind: BlockKind,
        _key: &[u8],
    ) -> Argv {
        args.to_argv()
    }

    /// Non-destructive readiness peek for a parked waiter: would replaying
    /// `serve_argv` (built by [`Self::block_serve_argv`], `$` already
    /// frozen) produce a reply right now? Runs on the key's owning shard
    /// when arming and is the gate for emitting a cross-shard wake. Must
    /// NOT mutate the store (no pop / no group-cursor advance). Default
    /// `false` so non-blocking embedders never spuriously wake.
    fn block_ready<A: ArgvView + ?Sized>(
        &self,
        _store: &mut Store,
        _serve_argv: &A,
        _kind: BlockKind,
    ) -> bool {
        false
    }

    /// Resolve all verb-dependent attributes in **one** verb-table lookup.
    /// The default implementation calls the per-attribute methods above
    /// (five upper_verb scans + matches); concrete impls SHOULD override
    /// this with a single match so the reactor's hot path pays the verb-
    /// resolution cost only once per command.
    fn resolve<A: ArgvView + ?Sized>(&self, args: &A) -> ResolvedCmd {
        ResolvedCmd {
            txn_kind: self.txn_kind(args),
            route: self.route(args),
            is_quit: self.is_quit(args),
            is_write: self.is_write(args),
            block_hint: self.block_hint(args),
            wake_idx: None,
        }
    }
}

