//! kevy-embedded — kevy without the network.
//!
//! In-process Redis-compatible key–value store: load + reply directly from
//! your own threads, no TCP, no shards, no reactor. Use this when you want
//! kevy's data structures + persistence in the same address space as your
//! app — caches, embedded databases, WASM blobs, sidecar tools.
//!
//! Zero crates.io dependencies: only `kevy-store` (the keyspace)
//! and `kevy-persist` (snapshot + AOF). The whole network layer
//! (`kevy-rt`, `kevy-sys`, `kevy-uring`) is intentionally NOT pulled in.
//!
//! # Quick start
//!
//! ```
//! use kevy_embedded::{Store, Config};
//!
//! # fn main() -> std::io::Result<()> {
//! let s = Store::open(Config::default())?;
//! s.set(b"greeting", b"hello")?;
//! assert_eq!(s.get(b"greeting")?, Some(b"hello".to_vec()));
//! # Ok(())
//! # }
//! ```
//!
//! # With persistence
//!
//! `with_persist(dir)` enables AOF auto-append on every write and replays
//! on `open` — restart-safe out of the box. Snapshot (`dump-0.rdb`) is
//! loaded first if present; AOF (`aof-0.aof`) is replayed on top.
//!
//! ```no_run
//! use kevy_embedded::{Store, Config};
//!
//! # fn main() -> std::io::Result<()> {
//! let s = Store::open(Config::default().with_persist("./data"))?;
//! s.set(b"counter", b"42")?;
//! drop(s); // flushes AOF on drop
//!
//! // Next process: state survives.
//! let s2 = Store::open(Config::default().with_persist("./data"))?;
//! assert_eq!(s2.get(b"counter")?, Some(b"42".to_vec()));
//! # Ok(())
//! # }
//! ```
//!
//! # When NOT to use this crate
//!
//! - You want a Redis-protocol TCP server → use the `kevy` crate's
//!   [`serve`](https://docs.rs/kevy/latest/kevy/fn.serve.html) instead.
//! - You need cross-process concurrency → kevy-embedded is single-process
//!   (one mutex). Multi-process needs the network layer.
#![forbid(unsafe_code)]

mod config;
mod info;
mod metric;
mod ops;
mod ops_bitmap;
mod ops_p2;
mod ops_p3;
mod pubsub;
mod reaper;
mod shard;
mod pubsub_bus;
mod replay;
#[cfg(not(target_arch = "wasm32"))]
mod replica_glue;
#[cfg(not(target_arch = "wasm32"))]
mod replica_runner;
#[cfg(not(target_arch = "wasm32"))]
mod replica_source;
mod store;
mod store_persist;

pub use config::{AppendFsync, Config, EvictionPolicy, TtlReaperMode};
pub use info::KevyInfo;
pub use metric::KevyMetric;
pub use kevy_persist::RewriteStats;
pub use kevy_store::{ExpireStats, ScoreBound, StoreError};
pub use pubsub::{PubsubFrame, Subscription};
pub use store::{Store, WeakStore};

/// Feed kevy's clocks on `wasm32-unknown-unknown`, which has neither
/// `Instant` nor `SystemTime`. Without a host-fed clock, TTL operations and
/// the reaper would trap. Call [`set_clock_ns`] (monotonic ns, e.g.
/// `Date.now() * 1e6`) before TTL-sensitive ops and once per `tick`, and
/// [`set_wall_clock_ms`] (Unix-epoch millis) if you use `XADD` auto-IDs or
/// `EXPIREAT`. No-ops conceptually on native targets — hence wasm-only.
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub use kevy_store::{set_clock_ns, set_wall_clock_ms};
