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
mod block_xshard_confirm;
#[cfg(debug_assertions)]
pub use block_xshard_confirm::counters as serve_counters;
mod block_xshard_registry;
mod block_xshard_target;
mod blocked;
mod lua_wake_bridge;
mod cache_padded;
mod client_ops;
mod cluster;
mod conn;
mod exec;
mod exec_build;
mod exec_client_intercept;
mod exec_crossslot;
mod exec_dispatch;
mod exec_fold;
mod exec_mutated;
mod exec_notify;
mod exec_op;
mod exec_pubsub;
mod exec_pubsub_pattern;
mod exec_listmove;
mod exec_rename;
mod exec_replwait;
mod exec_scan;
mod exec_feed;
mod exec_geostore;
mod exec_zalgebra;
mod exec_slowlog;
mod exec_watch;
mod inbox;
mod persist_worker;
pub mod propagation;
mod message;
mod message_kinds;
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
mod uring_io_write;
#[cfg(target_os = "linux")]
mod uring_ops;
#[cfg(target_os = "linux")]
mod uring_park;
#[cfg(target_os = "linux")]
mod uring_reactor;
#[cfg(target_os = "linux")]
mod uring_setup;
#[cfg(target_os = "linux")]
mod uring_stalldump;
#[cfg(any(target_os = "linux", test))] // `test` too: pure, tested everywhere
mod uring_write_linearize;

/// Hard cap on a single connection's accumulated unflushed reply
/// bytes. A client that stops reading (or a slow pub/sub subscriber)
/// lets its per-conn output buffer grow without bound; past this it is
/// disconnected so it can't OOM the shard. Deliberately generous
/// (512 MiB, one max bulk's order of magnitude): a legitimate large
/// reply drains progressively and never accumulates near it — only a
/// non-draining reader does. Enforced out-of-band per tick by
/// `Shard::enforce_output_limit` / `uring_enforce_output_limit`.
pub(crate) const CLIENT_OUTPUT_HARD_LIMIT: usize = 512 * 1024 * 1024;

pub use blocked::{BlockHint, BlockKind};
pub use lua_wake_bridge::push_lua_wake_key;
pub use reduce::shard_of as shard_of_key;
pub use cluster::shard_slot_range;
pub use exec_geostore::GeoHits;
pub use exec_slowlog::{SlowlogSub, parse_slowlog_sub};
pub use kevy_config::NotificationFlags;
pub use kevy_persist::Fsync;
pub use kevy_resp::{Argv, ArgvBorrowed, ArgvView, RespVersion};
pub use kevy_store::Store;
pub use replica_inbox::{
    ReplicaApply, ReplicaInboxReceiver, ReplicaInboxSender, SnapshotGate, replica_inbox_pair,
};
pub use replication_gate::ReplicatedApplyGuard;
pub use route::{Route, ScanArgs, XGroupCtx};
pub use client_ops::ClientKillFilter;
pub use message::{MultiOp, ZCombine};
pub use runtime::Runtime;
pub use types::{
    ExtensionReduced, LiveRuntimeConfig, NotifyClass, ReplicaAck, ReplicaViewRow, ResolvedCmd,
    TxnKind,
};

pub use crate::commands_trait::Commands;
mod commands_trait;


