//! Background-I/O thread — the orthodox valkey `bio.c` model in pure Rust.
//!
//! **Why this exists** (v1.25 A.3, B2 architecture per
//! `bench/V125-DECISIONS-PENDING.md`):
//!
//! Axis I 10 KB SET tail max sat at 130-160 ms in v1.25, isolated by
//! Phase A decomposition (`v125-deco-axis-i-c50-10kb.md` S09/S16) to the
//! synchronous `Drop` of overwritten `Value::ArcBulk(Arc<[u8]>)` — when the
//! Arc refcount hits zero, `Box::<[u8]>::drop` of a 10 KB jemalloc large-class
//! slot can stall on `madvise`/`munmap` for tens to hundreds of microseconds
//! (worst-case milliseconds when the slab consolidates). valkey solves this
//! identically via `lazyfree.c` — the dict overwrite enqueues the old
//! `robj` to a bio thread instead of `free()`-ing inline.
//!
//! G6 A2 (v1.25 Phase B, reverted in `bench/V125-AXIS-I-LATENCY.md`) tried
//! deferring drops to a per-shard `pending_drops: Vec<Value>` drained after
//! `flush_conn`. R3 ★ finding: that's WORSE (p999 +144 µs / 1 spike 64 ms),
//! because single-threaded deferred bunching converts the steady-state inline
//! drop into a periodic batched-drop stall *bigger* than the inlines it
//! replaced. The lesson is the same one valkey's lazyfree authors learned:
//! deferral without a separate thread carrying the work away is just a
//! rescheduling of the same critical-section cost. A real bio thread
//! actually removes the free from the reactor core's CPU budget.
//!
//! **Architecture** (B2 from the RFC table — single global thread, MPSC
//! `std::sync::mpsc`, work-item enum extensible to BGSAVE/BGREWRITEAOF
//! migration later):
//!
//! - One global thread for the whole `Runtime`, spawned in
//!   [`crate::Runtime::run`] BEFORE shards (so a shard's first overwrite
//!   already has a live consumer).
//! - `std::sync::mpsc::Sender<BioWork>` is `Clone + Send`; each shard
//!   gets a clone, then installs it on its `Store` via
//!   [`kevy_store::Store::set_bio_drop_sender`].
//! - The store's overwrite hot paths
//!   ([`kevy_store::Store::set_value_no_evict`] and the `maxmemory > 0`
//!   eviction-aware [`kevy_store::Store::set_value`]) take the old
//!   `Value` and `try_send` it to the bio thread when
//!   [`kevy_store::Value::is_heap_heavy`] is true. On a closed channel
//!   (bio thread joined → channel dropped — shouldn't happen mid-run)
//!   the value falls back to inline drop, preserving correctness.
//! - **Shutdown**: when [`crate::Runtime::run`] returns, the held
//!   `bio_send` field on the runtime is dropped. Once every cloned
//!   sender on every shard's `Store` is also dropped (shards joined),
//!   the channel closes, `recv()` returns `Err`, and the bio thread
//!   exits cleanly. The `JoinHandle` is `join()`-ed inside
//!   `Runtime::run` so the process doesn't exit while a free is in
//!   flight (correctness for `madvise` returning the page to the
//!   kernel before the process state is torn down).
//!
//! **Channel shape extension**: today the channel carries
//! `Vec<Box<kevy_store::Value>>` (v1.25 A.2 batch model — one mpsc
//! send per shard-flush, amortising the per-send atomic + cross-
//! thread cacheline cost across N drops). The follow-up uses are
//! wired by promoting this to a `BioWork` enum here; the per-shard
//! `BioSender` clone is already in place. Candidates (from
//! `bench/V125-DECISIONS-PENDING.md` A.3):
//! - `Save { view, snap_path, … }` — migrate `start_bg_save` off the
//!   per-shard `PersistWorker` mpsc onto this thread to consolidate
//!   resource use (the orthodox valkey model: one bio thread total).
//! - `RewriteAof { view, tmp }` — same migration for BGREWRITEAOF.
//! - `Fsync { aof_path }` — `appendfsync=always` durability without
//!   stalling the reactor on the `fdatasync` syscall.
//!
//! **CPU**: bio thread blocks on `recv()` — zero idle CPU. Each item is
//! the typical Linux `free()` of a ≤ 10s-KB Box, which the OS may or
//! may not return to the kernel (madvise) — single-digit µs amortised
//! per drop in steady state; the spike-killing property comes from
//! moving the wait OFF the reactor core.

use kevy_store::{BioDropSender, Value};
use std::sync::mpsc;
use std::thread;

/// Spawn the global bio thread and return `(sender, join_handle)`.
/// `Runtime::run` holds both: the sender is cloned into every shard's
/// `Store` via [`kevy_store::Store::set_bio_drop_sender`]; the handle
/// is `join()`-ed after the shard threads exit so the process doesn't
/// tear down while a free is still in flight.
///
/// **Channel shape**: the sender carries `Vec<Box<Value>>` — a batch
/// of heavy values produced by one shard since its last flush
/// (v1.25 A.2 batch-send model). The reactor calls
/// `Store::flush_pending_drops` at the end of every iter to push the
/// batch; the bio thread iterates the Vec and drops each `Box<Value>`.
/// Future extensions — `BGSAVE`/`BGREWRITEAOF` migration off
/// `PersistWorker`, `Fsync` off-thread for `appendfsync=always` — will
/// replace this with a `BioWork` enum carrying both `DropBatch(Vec<…>)`
/// and a `Save{…}` variant; the `BioDropSender` type alias on
/// `kevy-store` will then re-shape to `Sender<BioWork>`. Per
/// `bench/V125-DECISIONS-PENDING.md` A.3, those follow-ups share the
/// same single-thread B2 topology, so the call-site plumbing established
/// here (sender clone per shard, drop-on-shutdown channel close, join
/// on the held handle) is reused unchanged.
pub(crate) fn spawn() -> (BioDropSender, thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel::<Vec<Box<Value>>>();
    let handle = thread::Builder::new()
        .name("kevy-bio".to_string())
        .spawn(move || {
            // Blocking recv = zero idle CPU. Loop until every Sender
            // clone has been dropped (shards joined + runtime exits),
            // at which point `recv()` returns `Err` and we fall out.
            while let Ok(batch) = rx.recv() {
                // The interesting work is the implicit `Drop` of each
                // Box<Value> as the Vec is consumed (Box → Value →
                // ArcBulk → Box<[u8]> → free). Naming the binding
                // (rather than `drop(batch)` in one shot) keeps the
                // per-item Drop intent legible: we are the off-reactor
                // `free` for every item the shard handed us.
                for boxed in batch {
                    drop(boxed);
                }
            }
        })
        .expect("spawn kevy-bio thread");
    (tx, handle)
}
