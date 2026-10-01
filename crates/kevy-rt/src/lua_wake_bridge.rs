//! Bridge from Lua's `redis.call` writes to the runtime's blocked-
//! waiter wake hook.
//!
//! ## The problem
//!
//! When a Lua script calls `redis.call('XADD', ...)`, the kevy-side
//! dispatch closure in `kevy::cmd_lua` routes the call through
//! `Commands::dispatch_into(&mut Store, ...)` directly — that hits the
//! shard's `Store`, but it does NOT go through the runtime's
//! `commit_write` path which is where [`Shard::wake_key`] fires for
//! parked `BLPOP` / `BRPOP` / `XREAD BLOCK` / `BZPOPMIN` waiters.
//!
//! Net effect without the bridge: BullMQ Worker's `BZPOPMIN` on the
//! marker key, and `QueueEvents`' `XREAD BLOCK` on the events stream,
//! both fail to wake when an EVAL script writes the trigger value.
//! Jobs still complete (visible in `getJobCounts`) but the wake-driven
//! pipeline is missing, leaving listeners hanging until their own
//! timeout cycle.
//!
//! ## The bridge
//!
//! Pure thread-local buffer. The dispatch closure pushes affected
//! write keys here after each wake-triggering `redis.call`; the
//! runtime drains and fires `wake_key` for each after the EVAL
//! returns. Single-threaded per shard so a `Cell<Vec<...>>` would
//! suffice, but `RefCell<Vec<...>>` gives a cleaner `take` shape.
//!
//! Zero overhead when no Lua write happens this dispatch: a push also
//! sets the propagation module's armed flag, and the post-write step
//! only drains when that flag is set.

use std::cell::RefCell;

thread_local! {
    static LUA_WAKE_BUFFER: RefCell<Vec<Vec<u8>>> = const { RefCell::new(Vec::new()) };
}

/// Lua's `redis.call` dispatch closure calls this after every
/// wake-triggering write (LPUSH / RPUSH / XADD / ZADD / ZINCRBY).
/// The runtime drains the buffer after the outer
/// EVAL dispatch returns and fires `wake_key` for each.
///
/// Cheap: one thread-local lookup + one `Vec::push` per call, plus the
/// flag that tells the post-write step to drain.
///
/// ```
/// // inside a `redis.call` dispatch closure, after a write that can wake a
/// // parked BLPOP / XREAD BLOCK / BZPOPMIN on this key
/// let mut store = kevy_rt::Store::new();
/// let n = store.lpush(b"jobs", &[&b"j1"[..]])?;
/// kevy_rt::push_lua_wake_key(b"jobs");
/// assert_eq!(n, 1);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn push_lua_wake_key(key: &[u8]) {
    LUA_WAKE_BUFFER.with(|b| b.borrow_mut().push(key.to_vec()));
    crate::propagation::arm();
}

/// Drain the per-shard Lua wake buffer. The runtime calls this once
/// after every top-level command dispatch (the EVAL/EVALSHA case is
/// what fills it; every other verb's drain is a single empty-check).
pub(crate) fn drain_lua_wake_buffer() -> Vec<Vec<u8>> {
    LUA_WAKE_BUFFER.with(|b| std::mem::take(&mut *b.borrow_mut()))
}
