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
//! # fn main() -> kevy_embedded::KevyResult<()> {
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
//! # fn main() -> kevy_embedded::KevyResult<()> {
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
//!
//! # Locking & concurrency
//!
//! The keyspace is split into `Config::shards` independent shards, each a
//! `kevy_store::Store` behind its own `RwLock` (default: **1 shard** = one
//! lock over the whole keyspace). A key maps to its shard by hash; writes take
//! that shard's exclusive lock.
//!
//! **Reads take a *shared* per-shard lock where it's sound to.** `GET` (and the
//! FFI zero-copy `get_shared` lane) use the shared lock whenever the active
//! eviction policy won't consume a per-read LRU/LFU tick — `maxmemory == 0`
//! (the default), or the `NoEviction` / `*Random` / `VolatileTtl` policies. The
//! true LRU/LFU policies (`*Lru` / `*Lfu`) instead take the exclusive lock so
//! each access stamps the clock the eviction scorer ranks by. Read-only
//! aggregations (`DBSIZE`, `used_memory`, the `INFO` counters) likewise take
//! shared locks so a full-keyspace scan doesn't stall concurrent writers.
//!
//! This is a **lock-correctness** property, not a throughput one: a read-only
//! operation doesn't hold the exclusive lock against a concurrent writer on its
//! shard. It is **not** a lock-free read path — concurrent readers still
//! contend on the shard's `RwLock` word (a shared cache line), so read scaling
//! is bounded by **shard count**, not core count. To spread read/write
//! contention across cores, raise `Config::shards`.
//!
//! **Known limitation:** the sibling reads (`hget`, `exists`, `smembers`,
//! `zscore`, `llen`, `scard`, `zcard`, `type_of`, `ttl_ms`, …) currently still
//! take the shard's *write* lock even though the underlying keyspace methods
//! are read-only. Moving them onto the shared lane is a tracked follow-up
//! (bench-gated separately from the `GET` lane above).
//!
//! # Cargo features
//!
//! `default` is the full surface. For constrained targets (IoT / edge)
//! cut it down with `default-features = false, features = [...]`:
//!
//! | feature | adds |
//! |---------|------|
//! | `core` | in-memory KV + TTL + pub/sub + pipeline/atomic (the minimal base) |
//! | `persist` | snapshot + AOF durability (`with_persist`, replay on open) |
//! | `index` | secondary indexes + views |
//! | `text` | full-text index segments (implies `index`) |
//! | `vector` | HNSW vector index segments (implies `index`) |
//! | `replicate` | embed-as-replica / embed-as-writer + CDC feed (implies `persist`) |
//! | `listener` | the read-only RESP listener |
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod config;
mod dispatch;
mod info;
// Unconditional: `OpenReport` rides the DropGuard and the Store
// handle in every archetype (a no-persist open reports zeros); only
// the sink WIRING stays persist-gated in config.rs.
mod metric;
mod ops;
mod ops_atomic;
mod ops_atomic_all;
mod ops_atomic_all_reads;
mod ops_reconcile;
#[cfg(feature = "index")]
mod ops_atomic_all_index;
mod ops_bitmap;
mod ops_bonus;
mod ops_keyspace;
mod ops_more;
mod ops_p2;
mod ops_p3;
mod ops_pipeline;
#[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
mod ops_feed;
mod ops_blocking;
mod ops_hash_ttl;
#[cfg(feature = "index")]
mod ops_index;
#[cfg(feature = "index")]
mod ops_index_sync;
#[cfg(feature = "index")]
mod ops_index_cold;
#[cfg(all(feature = "index", feature = "persist", not(target_arch = "wasm32")))]
mod ops_index_window;
#[cfg(feature = "index")]
mod ops_table;
#[cfg(feature = "index")]
mod ops_view;
#[cfg(all(feature = "listener", not(target_arch = "wasm32")))]
mod listener;
mod ops_snapshot_view;
mod ops_zset_algebra;
mod ops_zset_flags;
mod op_manifest;
mod store_glue;
mod ops_scan;
pub use ops_atomic::AtomicCtx;
pub use ops_atomic_all::AtomicAllShards;
pub use ops_bitmap::BitOp;
pub use ops_pipeline::Pipeline;
mod pubsub;
mod reaper;
mod shard;
mod pubsub_bus;
#[cfg(feature = "persist")]
mod replay;
#[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
mod replica_glue;
#[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
mod replica_runner;
#[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
mod replica_source;
mod store;
mod store_inner;
mod store_wire;
#[cfg(feature = "persist")]
mod store_persist;

pub use config::{Config, EvictionPolicy, TtlReaperMode};
#[cfg(feature = "tier")]
pub use config::TierBudgetSpec;
#[cfg(feature = "tier")]
mod config_tier;
#[cfg(feature = "persist")]
pub use config::AppendFsync;
pub use info::{KevyInfo, KevyTierInfo};
#[cfg(feature = "persist")]
pub use metric::KevyMetric;
pub use metric::OpenReport;
#[cfg(feature = "persist")]
pub use kevy_persist::RewriteStats;
pub use kevy_store::{
    ExpireStats, GetShared, HExpireCode, HExpireCond, KevyError, KevyResult, ScoreBound,
    StoreError, ZAggregate, ZaddFlags, ZaddReport,
};
#[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
pub use ops_feed::{Change, ChangeBatch, FeedError, PrefixInfo};
pub use ops_snapshot_view::{Snapshot, SnapshotEntry};
pub use ops_reconcile::ReconcileReport;
#[cfg(feature = "index")]
pub use ops_index::IndexPage;
#[cfg(feature = "text")]
pub use ops_index::highlight::{FacetCounts, MatchOpts, MatchPage};
#[cfg(feature = "index")]
pub use ops_index::claused::{ScalarPage, ScalarQueryOpts, ValueFilter};
#[cfg(feature = "index")]
pub use ops_view::ViewPage;
#[cfg(feature = "index")]
pub use kevy_index::{AggBy, AnnSpec, GroupStats, Leaf as ViewLeaf, Tree as ViewTree, ViewMode};
#[cfg(feature = "index")]
pub use kevy_index::{Cursor as IndexCursor, IndexKind, IndexValue, SegmentStats as IndexStats, ValType as IndexValType};
// The TABLE face — the dogfood report's F7: `Store::table_declare` takes
// a `TableSpec` the facade did not export, so the typed face of a
// flagship v4 feature was uncallable without depending on kevy-index
// directly. The consumer gate (tools/facadegate) now builds against
// these from outside the workspace, which is what would have caught it.
#[cfg(feature = "index")]
pub use kevy_index::{IndexVerify, OrderPath, TableEnsure, TableIndex, TableSpec, TableVerify};
// `each_prefix` hands the callback a `kevy_store::Value` — same class of
// gap: a public signature whose type the facade could not name.
pub use kevy_store::Value;
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
