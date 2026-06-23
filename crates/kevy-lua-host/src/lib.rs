//! Scoped-borrow bridge between [`kevy_lua::Bridge`] and a host-owned
//! mutable shard state (`Store`, `KeyspaceStore`, anything `'static`).
//!
//! `kevy-lua`'s dispatch closure type is
//! `Fn(&[&[u8]], bool) -> Vec<u8> + 'static`. The `'static` bound is
//! mandatory — luna stores the closure as Vm userdata (`Any + 'static`).
//! That makes it impossible to capture `&mut T` directly. This crate
//! offers a tiny `LuaHost<T>` wrapper that re-introduces the borrow via
//! a scoped thread-local pointer set inside `LuaHost::eval` and cleared
//! right after. The dispatch closure consults the pointer.
//!
//! ## Safety contract (read this if you touch the unsafe)
//!
//! - `LuaHost<T>` parameterises over the host context type `T` (kevy's
//!   `Store` for the production wiring; an arbitrary type in tests).
//! - `LuaHost::new(dispatch_fn)` builds a kevy-lua `Bridge` whose
//!   dispatch closure does `with_current::<T>(|t| dispatch_fn(t, argv, ro))`.
//!   The closure carries NO captured state of its own — it just reads
//!   the scoped pointer.
//! - `LuaHost::eval(&mut self, &mut T, …)` (and friends) set
//!   `CURRENT_T = ctx as *mut T` BEFORE delegating to
//!   `Bridge::eval`, and CLEAR `CURRENT_T = null` after. A `Drop`
//!   guard ensures the clear even on panic.
//! - Inside the dispatch closure, `with_current` dereferences
//!   `CURRENT_T` exactly once per call. The pointer is only ever
//!   non-null while the outer `&mut T` is borrowed mutably by
//!   `LuaHost::eval`, so no aliasing exists.
//! - kevy is single-threaded per-shard — every shard owns its own
//!   `LuaHost<T>` and runs on a dedicated thread. The thread-local
//!   gives correct isolation without any synchronisation overhead.
//!
//! The unsafe footprint is **one** `unsafe { &mut *p }` inside
//! `with_current` plus the `Cell::set(ptr)` ergonomics. Audited per
//! every kevy v1.27+ commit touching this file.

#![doc(html_no_source)]

use kevy_lua::{Bridge, FlushMode, Reply, ScriptSha1};
use std::cell::Cell;
use std::marker::PhantomData;

/// Type-erased host context pointer. Set per-call from `LuaHost::eval`
/// and friends, cleared by the [`ResetCurrent`] RAII guard. The
/// `usize` type is just "address-sized opaque": we cast to `*mut T`
/// inside [`with_current`] under the safety contract documented at
/// the crate root.
#[doc(hidden)]
pub type CurrentTag = usize;

thread_local! {
    /// Per-thread scoped pointer to the host context, encoded as a
    /// raw address.
    static CURRENT: Cell<CurrentTag> = const { Cell::new(0) };
}

struct ResetCurrent {
    prev: CurrentTag,
}

impl Drop for ResetCurrent {
    fn drop(&mut self) {
        CURRENT.with(|c| c.set(self.prev));
    }
}

fn set_current<T>(ctx: &mut T) -> ResetCurrent {
    let new_addr = ctx as *mut T as usize;
    let prev = CURRENT.with(|c| {
        let p = c.get();
        c.set(new_addr);
        p
    });
    ResetCurrent { prev }
}

/// Run `f` with a mutable borrow of the currently-set host context.
/// Returns `None` if `LuaHost::eval` isn't on the stack.
///
/// Used inside the dispatch fn passed to [`LuaHost::new`] — call once
/// per `redis.call`, do the kevy dispatch, return RESP bytes.
pub fn with_current<T: 'static, R>(f: impl FnOnce(&mut T) -> R) -> Option<R> {
    let addr = CURRENT.with(Cell::get);
    if addr == 0 {
        return None;
    }
    // SAFETY: see crate-level docs. The pointer was installed by
    // `set_current(&mut T)` whose `&mut T` borrow is held for the
    // duration of `LuaHost::eval` (which is the only call path that
    // reaches user dispatch code). Single-threaded per shard, so no
    // aliasing across threads either.
    let r = unsafe { &mut *(addr as *mut T) };
    Some(f(r))
}

/// kevy-side per-shard Lua host. Wraps a [`kevy_lua::Bridge`] plus
/// the scoped-pointer plumbing.
///
/// `T` is whatever shard state the dispatch closure needs (`Store`,
/// `KeyspaceStore`, …). It must outlive every `LuaHost::eval` call
/// (trivially true: kevy holds the `&mut T` while delegating).
pub struct LuaHost<T: 'static> {
    bridge: Bridge,
    _marker: PhantomData<fn() -> T>,
}

impl<T: 'static> LuaHost<T> {
    /// Build a host with `dispatch_fn` as the redis.call backend.
    ///
    /// `dispatch_fn` receives a `&mut T` (the current shard context),
    /// the script's argv (command + args), and the `read_only` flag.
    /// It must return RESP reply bytes — production callers route to
    /// kevy's dispatch, tests just return canned replies.
    ///
    /// The closure receives `&mut T` via [`with_current`], so it
    /// must be `Fn(&mut T, …) -> …` rather than `FnMut`. (kevy's
    /// dispatch path is `&mut self`, so `Fn(&mut T, …)` is exactly
    /// what we need.)
    pub fn new<F>(dispatch_fn: F) -> Self
    where
        F: Fn(&mut T, &[&[u8]], bool) -> Vec<u8> + 'static,
    {
        let bridge = Bridge::new(move |argv, ro| {
            with_current::<T, _>(|t| dispatch_fn(t, argv, ro))
                .unwrap_or_else(|| {
                    b"-ERR kevy-lua-host: dispatch called outside an active eval scope\r\n"
                        .to_vec()
                })
        });
        LuaHost {
            bridge,
            _marker: PhantomData,
        }
    }

    /// Run a script. Scoped-installs `ctx` so the dispatch closure
    /// can find it via [`with_current`], then delegates to
    /// `Bridge::eval`.
    pub fn eval(
        &mut self,
        ctx: &mut T,
        script: &[u8],
        keys: &[&[u8]],
        args: &[&[u8]],
    ) -> Reply {
        let _guard = set_current(ctx);
        self.bridge.eval(script, keys, args)
    }

    /// Read-only counterpart of [`Self::eval`].
    pub fn eval_ro(
        &mut self,
        ctx: &mut T,
        script: &[u8],
        keys: &[&[u8]],
        args: &[&[u8]],
    ) -> Reply {
        let _guard = set_current(ctx);
        self.bridge.eval_ro(script, keys, args)
    }

    /// Run a previously-loaded script by SHA1.
    pub fn evalsha(
        &mut self,
        ctx: &mut T,
        sha1: ScriptSha1,
        keys: &[&[u8]],
        args: &[&[u8]],
    ) -> Reply {
        let _guard = set_current(ctx);
        self.bridge.evalsha(sha1, keys, args)
    }

    /// Read-only `EVALSHA`.
    pub fn evalsha_ro(
        &mut self,
        ctx: &mut T,
        sha1: ScriptSha1,
        keys: &[&[u8]],
        args: &[&[u8]],
    ) -> Reply {
        let _guard = set_current(ctx);
        self.bridge.evalsha_ro(sha1, keys, args)
    }

    /// SCRIPT LOAD — cache without running. No context needed.
    pub fn script_load(&mut self, script: &[u8]) -> ScriptSha1 {
        self.bridge.script_load(script)
    }

    /// SCRIPT EXISTS.
    #[must_use]
    pub fn script_exists(&self, sha1s: &[ScriptSha1]) -> Vec<bool> {
        self.bridge.script_exists(sha1s)
    }

    /// SCRIPT FLUSH.
    pub fn script_flush(&mut self, mode: FlushMode) {
        self.bridge.script_flush(mode);
    }

    /// Forward [`kevy_lua::Bridge::set_instr_budget`] — set the
    /// per-Vm instruction cap. The operator wires `[lua]
    /// time_limit_ms` here at server startup.
    pub fn set_instr_budget(&mut self, n: i64) {
        self.bridge.set_instr_budget(n);
    }

    /// Forward [`kevy_lua::Bridge::set_allowed_dialects`].
    pub fn set_allowed_dialects(&mut self, versions: &[kevy_lua::LuaVersion]) {
        self.bridge.set_allowed_dialects(versions);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A toy shard-state type. Carries a small in-memory keyspace plus a
    /// dispatch implementation analogous to kevy's command path.
    #[derive(Default)]
    struct ToyStore {
        kv: std::collections::HashMap<Vec<u8>, Vec<u8>>,
        calls_seen: u32,
    }

    impl ToyStore {
        fn run(&mut self, argv: &[&[u8]], read_only: bool) -> Vec<u8> {
            self.calls_seen += 1;
            if argv.is_empty() {
                return b"-ERR no command\r\n".to_vec();
            }
            let cmd: Vec<u8> = argv[0].iter().map(|b| b.to_ascii_uppercase()).collect();
            // Toy write-flag table (matches kevy's `is_write_verb`
            // shape; production wiring delegates to kevy::cmd).
            let is_write = matches!(cmd.as_slice(), b"SET" | b"DEL");
            if read_only && is_write {
                return b"-READONLY can't write against a read-only script\r\n".to_vec();
            }
            match cmd.as_slice() {
                b"SET" => {
                    self.kv.insert(argv[1].to_vec(), argv[2].to_vec());
                    b"+OK\r\n".to_vec()
                }
                b"GET" => match self.kv.get(argv[1]) {
                    Some(v) => {
                        let mut out = format!("${}\r\n", v.len()).into_bytes();
                        out.extend_from_slice(v);
                        out.extend_from_slice(b"\r\n");
                        out
                    }
                    None => b"$-1\r\n".to_vec(),
                },
                b"DEL" => {
                    let n = self.kv.remove(argv[1]).is_some() as i64;
                    format!(":{n}\r\n").into_bytes()
                }
                _ => b"-ERR unknown\r\n".to_vec(),
            }
        }
    }

    fn make_host() -> LuaHost<ToyStore> {
        LuaHost::<ToyStore>::new(|store, argv, ro| store.run(argv, ro))
    }

    #[test]
    fn eval_calls_dispatch_with_live_store() {
        let mut host = make_host();
        let mut store = ToyStore::default();
        let reply = host.eval(
            &mut store,
            b"redis.call('SET', KEYS[1], ARGV[1])\n\
              return redis.call('GET', KEYS[1])\n",
            &[b"k"],
            &[b"hello"],
        );
        assert_eq!(reply, b"$5\r\nhello\r\n");
        assert_eq!(store.kv.get(&b"k".to_vec()), Some(&b"hello".to_vec()));
        assert_eq!(store.calls_seen, 2);
    }

    #[test]
    fn eval_ro_blocks_writes() {
        let mut host = make_host();
        let mut store = ToyStore::default();
        let reply = host.eval_ro(
            &mut store,
            b"return redis.call('SET', KEYS[1], 'v')",
            &[b"k"],
            &[],
        );
        assert!(reply.starts_with(b"-READONLY "));
        assert!(!store.kv.contains_key(&b"k".to_vec()));
    }

    #[test]
    fn evalsha_round_trip() {
        let mut host = make_host();
        let mut store = ToyStore::default();
        let sha = host.script_load(b"return redis.call('GET', KEYS[1])");
        store.kv.insert(b"x".to_vec(), b"42".to_vec());
        let reply = host.evalsha(&mut store, sha, &[b"x"], &[]);
        assert_eq!(reply, b"$2\r\n42\r\n");
    }

    #[test]
    fn dispatch_outside_scope_is_a_clear_error() {
        // No active `host.eval()` → `with_current` returns None and
        // the dispatch returns the documented -ERR reply.
        let r = with_current::<ToyStore, _>(|_| 1);
        assert!(r.is_none());
    }

    #[test]
    fn pointer_is_cleared_after_eval_returns() {
        let mut host = make_host();
        let mut store = ToyStore::default();
        let _ = host.eval(&mut store, b"return 1", &[], &[]);
        // After eval returns, CURRENT has been reset.
        let r = with_current::<ToyStore, _>(|_| 1);
        assert!(r.is_none());
    }

    #[test]
    fn nested_eval_calls_restore_outer_context() {
        // Set CURRENT to a sentinel address, call host.eval (which
        // pushes its own), confirm the sentinel comes back after.
        let sentinel_addr: usize = 0xdead_beef;
        CURRENT.with(|c| c.set(sentinel_addr));
        let mut host = make_host();
        let mut store = ToyStore::default();
        let _ = host.eval(&mut store, b"return 1", &[], &[]);
        let restored = CURRENT.with(Cell::get);
        assert_eq!(restored, sentinel_addr);
        CURRENT.with(|c| c.set(0));
    }
}

#[cfg(test)]
mod p7e_tests {
    use super::*;

    /// P7e — set_instr_budget on a busy-ish loop. Default budget is
    /// 200 M (5 s on modern hardware); we shrink to 100 instructions
    /// and confirm a 10 000-iter loop trips the budget. Then we
    /// flush and run the same script under unlimited (0) budget to
    /// confirm the setter is live.
    #[test]
    fn instr_budget_trips_on_long_loop() {
        let mut host = LuaHost::<()>::new(|_ctx, _argv, _ro| Vec::new());
        host.set_instr_budget(100); // very tight cap
        let mut nothing = ();
        let reply = host.eval(
            &mut nothing,
            b"local s = 0\nfor i = 1, 10000 do s = s + i end\nreturn s",
            &[],
            &[],
        );
        // Budget exceeded → luna surfaces an error → bridge wraps in
        // -ERR. Don't be picky about the exact wording — just confirm
        // it's an error, not an integer result.
        assert!(
            reply.starts_with(b"-ERR "),
            "expected -ERR budget reply, got: {:?}",
            String::from_utf8_lossy(&reply)
        );
    }

    #[test]
    fn unlimited_budget_runs_to_completion() {
        let mut host = LuaHost::<()>::new(|_ctx, _argv, _ro| Vec::new());
        host.set_instr_budget(0); // unlimited
        let mut nothing = ();
        let reply = host.eval(
            &mut nothing,
            b"local s = 0\nfor i = 1, 10000 do s = s + i end\nreturn s",
            &[],
            &[],
        );
        // 1+...+10000 = 50005000
        assert_eq!(reply, b":50005000\r\n");
    }
}
