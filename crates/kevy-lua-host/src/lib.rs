//! Scoped-borrow bridge between [`kevy_lua::Bridge`] and a host-owned
//! mutable shard state (`Store`, `KeyspaceStore`, anything `'static`).
//!
//! `kevy-lua`'s dispatch closure type is
//! `Fn(&[&[u8]], bool) -> Vec<u8> + 'static`. The `'static` bound is
//! mandatory — luna-core stores the closure as Vm userdata (`Any + 'static`).
//! That makes it impossible to capture `&mut T` directly. This crate
//! offers a tiny `LuaHost<T>` wrapper that re-introduces the borrow via
//! a scoped thread-local pointer set inside `LuaHost::eval` and cleared
//! right after. The dispatch closure consults the pointer.
//!
//! ```
//! use kevy_lua_host::LuaHost;
//!
//! // the host state the script's redis.call reaches: here, a call counter
//! let mut host = LuaHost::<u32>::new(|calls, _argv, _read_only| {
//!     *calls += 1;
//!     b":7\r\n".to_vec()
//! });
//! let mut calls = 0;
//! host.eval(&mut calls, b"return redis.call('GET', KEYS[1])", &[b"k"], &[]);
//! assert_eq!(calls, 1);
//! ```
//!
//! ## Safety contract (read this if you touch the unsafe)
//!
//! - `LuaHost<T>` parameterises over the host context type `T` (kevy's
//!   `Store` for the production wiring; an arbitrary type in tests).
//! - `LuaHost::new(dispatch_fn)` builds a kevy-lua `Bridge` whose
//!   dispatch closure does `with_current::<T>(|t| dispatch_fn(t, argv, ro))`.
//!   The closure carries NO captured state of its own — it just reads
//!   the scoped pointer.
//! - `LuaHost::eval(&mut self, &mut T, …)` (and friends) install
//!   `(ctx as *mut T, TypeId::of::<T>())` BEFORE delegating to
//!   `Bridge::eval`, and restore the previous slot after. A `Drop`
//!   guard ensures the restore even on panic.
//! - `with_current::<T>` dereferences the pointer only when the slot's
//!   type id is `T`'s, and empties the slot while its closure runs. The
//!   pointer is only ever installed while the outer `&mut T` is borrowed
//!   mutably by `LuaHost::eval`, and lent to one closure at a time, so
//!   no aliasing exists.
//! - kevy is single-threaded per-shard — every shard owns its own
//!   `LuaHost<T>` and runs on a dedicated thread. The thread-local
//!   gives correct isolation without any synchronisation overhead.
//!
//! The unsafe footprint is **one** `unsafe { &mut *p }` inside
//! `with_current` plus the `Cell::set(ptr)` ergonomics. Audit it on
//! every commit touching this file.

#![doc(html_no_source)]
#![warn(missing_docs)]

use kevy_lua::{Bridge, FlushMode, Reply, ScriptSha1};
use std::any::TypeId;
use std::cell::Cell;
use std::marker::PhantomData;

/// The host context installed for the running eval: its address and the
/// type it was installed as. `None` outside an eval, and while
/// [`with_current`] has the context lent out.
type Current = Option<(usize, TypeId)>;

thread_local! {
    /// Per-thread scoped pointer to the host context.
    static CURRENT: Cell<Current> = const { Cell::new(None) };
}

/// Puts back what [`CURRENT`] held before, on every exit path.
struct ResetCurrent {
    prev: Current,
}

impl Drop for ResetCurrent {
    fn drop(&mut self) {
        CURRENT.with(|c| c.set(self.prev));
    }
}

fn set_current<T: 'static>(ctx: &mut T) -> ResetCurrent {
    let now = Some((ctx as *mut T as usize, TypeId::of::<T>()));
    ResetCurrent { prev: CURRENT.with(|c| c.replace(now)) }
}

thread_local! {
    /// Per-thread parked host for [`with_thread_host`], type-erased so
    /// one slot serves any `T`.
    static THREAD_HOST: std::cell::RefCell<Option<Box<dyn std::any::Any>>> =
        const { std::cell::RefCell::new(None) };
}

/// Run `f` against this thread's lazily-built [`LuaHost<T>`].
///
/// A `LuaHost` is `!Send` — luna-core's `Vm` holds `Rc`s and raw GC
/// pointers — so a thread-per-core server cannot park it inside its
/// `Send` per-shard command value. This slot keeps one host per shard
/// *thread* instead: identical isolation (thread == shard), owned by
/// the crate that knows why the type can't travel.
///
/// `build` runs once, on the first call on this thread. Returns
/// `None` when the slot is already borrowed — a re-entrant eval on
/// the same thread; callers surface their nested-eval error. A parked
/// host of a *different* `T` (mixed test harnesses; production uses
/// one `T` per process) is dropped and rebuilt.
///
/// ```
/// use kevy_lua_host::{LuaHost, with_thread_host};
/// let build = || LuaHost::<u32>::new(|n, _argv, _ro| format!(":{n}\r\n").into_bytes());
/// let mut ctx = 5;
/// let load = |h: &mut LuaHost<u32>| h.script_load(b"return redis.call('GET', 'k')");
/// let sha = with_thread_host(build, load).ok_or("re-entrant eval")?;
/// // the second call finds the same host, script cache included
/// let reply = with_thread_host(build, |h| h.evalsha(&mut ctx, sha, &[], &[]));
/// assert_eq!(reply.ok_or("re-entrant eval")?, b":5\r\n");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn with_thread_host<T: 'static, R>(
    build: impl FnOnce() -> LuaHost<T>,
    f: impl FnOnce(&mut LuaHost<T>) -> R,
) -> Option<R> {
    THREAD_HOST.with(|slot| {
        let mut g = slot.try_borrow_mut().ok()?;
        if !g.as_ref().is_some_and(|b| b.is::<LuaHost<T>>()) {
            *g = Some(Box::new(build()));
        }
        let host = g
            .as_mut()
            .expect("slot filled above")
            .downcast_mut::<LuaHost<T>>()
            .expect("type matched or rebuilt above");
        Some(f(host))
    })
}

/// Run `f` with a mutable borrow of the currently-set host context.
///
/// Returns `None` when no `LuaHost::eval` is on the stack, when the
/// running eval's context is not a `T`, or when an enclosing
/// `with_current` already holds it — the context is lent to one caller
/// at a time, so two `&mut T` to it never exist.
///
/// Used inside the dispatch fn passed to [`LuaHost::new`] — call once
/// per `redis.call`, do the kevy dispatch, return RESP bytes.
///
/// ```
/// use kevy_lua_host::{LuaHost, with_current};
/// assert_eq!(with_current::<u32, _>(|n| *n), None, "no eval is running");
/// let mut host = LuaHost::<u32>::new(|n, _argv, _ro| {
///     *n += 1;
///     assert_eq!(with_current::<u32, _>(|_| ()), None, "already lent to this call");
///     b"+OK\r\n".to_vec()
/// });
/// let mut calls = 0u32;
/// host.eval(&mut calls, b"return redis.call('PING')", &[], &[]);
/// assert_eq!(calls, 1);
/// ```
pub fn with_current<T: 'static, R>(f: impl FnOnce(&mut T) -> R) -> Option<R> {
    let (addr, ty) = CURRENT.with(Cell::get)?;
    if ty != TypeId::of::<T>() {
        return None;
    }
    let _lent = ResetCurrent { prev: CURRENT.with(|c| c.replace(None)) };
    // SAFETY: see crate-level docs. The address was installed by
    // `set_current::<T>` (the type id matched) from a `&mut T` that
    // `LuaHost::eval` holds for as long as the address is installed, and
    // the slot is empty while `f` runs, so this is the only live `&mut T`.
    // Single-threaded per shard, so no aliasing across threads either.
    let r = unsafe { &mut *(addr as *mut T) };
    Some(f(r))
}

/// kevy-side per-shard Lua host. Wraps a [`kevy_lua::Bridge`] plus
/// the scoped-pointer plumbing.
///
/// `T` is whatever shard state the dispatch closure needs (`Store`,
/// `KeyspaceStore`, …). It must outlive every `LuaHost::eval` call
/// (trivially true: kevy holds the `&mut T` while delegating).
///
/// ```
/// use std::collections::HashMap;
/// use kevy_lua_host::LuaHost;
///
/// type Keyspace = HashMap<Vec<u8>, Vec<u8>>;
/// let mut host = LuaHost::<Keyspace>::new(|ks, argv, _ro| {
///     ks.insert(argv[1].to_vec(), argv[2].to_vec());
///     b"+OK\r\n".to_vec()
/// });
/// let mut shard = Keyspace::new();
/// host.eval(&mut shard, b"return redis.call('SET', KEYS[1], ARGV[1])", &[b"k"], &[b"v"]);
/// assert_eq!(shard.get(&b"k"[..]), Some(&b"v".to_vec()));
/// ```
#[derive(Debug)]
pub struct LuaHost<T> {
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
    ///
    /// ```
    /// // the context here is a log of every command the script sent
    /// let mut host = kevy_lua_host::LuaHost::<Vec<Vec<u8>>>::new(|log, argv, _ro| {
    ///     log.push(argv[0].to_vec());
    ///     b"+OK\r\n".to_vec()
    /// });
    /// let mut log = Vec::new();
    /// host.eval(&mut log, b"redis.call('PING') return redis.call('ECHO')", &[], &[]);
    /// assert_eq!(log, [b"PING".to_vec(), b"ECHO".to_vec()]);
    /// ```
    pub fn new<F>(dispatch_fn: F) -> Self
    where
        F: Fn(&mut T, &[&[u8]], bool) -> Vec<u8> + 'static,
    {
        let bridge = Bridge::new(move |argv, ro| {
            with_current::<T, _>(|t| dispatch_fn(t, argv, ro)).unwrap_or_else(|| {
                b"-ERR kevy-lua-host: dispatch called outside an active eval scope\r\n".to_vec()
            })
        });
        LuaHost { bridge, _marker: PhantomData }
    }

    /// Run a script. Scoped-installs `ctx` so the dispatch closure
    /// can find it via [`with_current`], then delegates to
    /// `Bridge::eval`.
    ///
    /// ```
    /// let mut host = kevy_lua_host::LuaHost::<i64>::new(|n, _argv, _ro| {
    ///     *n += 1;
    ///     format!(":{n}\r\n").into_bytes()
    /// });
    /// let mut counter = 10;
    /// let reply = host.eval(&mut counter, b"return redis.call('INCR', KEYS[1])", &[b"c"], &[]);
    /// assert_eq!(reply, b":11\r\n");
    /// assert_eq!(counter, 11);
    /// ```
    pub fn eval(&mut self, ctx: &mut T, script: &[u8], keys: &[&[u8]], args: &[&[u8]]) -> Reply {
        let _guard = set_current(ctx);
        self.bridge.eval(script, keys, args)
    }

    /// Read-only counterpart of [`Self::eval`]: the dispatch closure
    /// receives `read_only = true`.
    ///
    /// ```
    /// let mut host = kevy_lua_host::LuaHost::<Vec<bool>>::new(|seen, _argv, ro| {
    ///     seen.push(ro);
    ///     b"+OK\r\n".to_vec()
    /// });
    /// let mut seen = Vec::new();
    /// host.eval(&mut seen, b"return redis.call('GET', 'k')", &[], &[]);
    /// host.eval_ro(&mut seen, b"return redis.call('GET', 'k')", &[], &[]);
    /// assert_eq!(seen, [false, true]);
    /// ```
    pub fn eval_ro(&mut self, ctx: &mut T, script: &[u8], keys: &[&[u8]], args: &[&[u8]]) -> Reply {
        let _guard = set_current(ctx);
        self.bridge.eval_ro(script, keys, args)
    }

    /// Run a previously-loaded script by SHA1.
    ///
    /// ```
    /// let mut host = kevy_lua_host::LuaHost::<()>::new(|_, _argv, _ro| Vec::new());
    /// let sha = host.script_load(b"return tonumber(ARGV[1]) * 2");
    /// assert_eq!(host.evalsha(&mut (), sha, &[], &[b"21"]), b":42\r\n");
    /// assert!(host.evalsha(&mut (), [0; 20], &[], &[]).starts_with(b"-NOSCRIPT"));
    /// ```
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

    /// Read-only `EVALSHA`: the dispatch closure receives
    /// `read_only = true`.
    ///
    /// ```
    /// let mut host = kevy_lua_host::LuaHost::<Vec<bool>>::new(|seen, _argv, ro| {
    ///     seen.push(ro);
    ///     b"+OK\r\n".to_vec()
    /// });
    /// let sha = host.script_load(b"return redis.call('GET', 'k')");
    /// let mut seen = Vec::new();
    /// host.evalsha_ro(&mut seen, sha, &[], &[]);
    /// assert_eq!(seen, [true]);
    /// ```
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
    ///
    /// ```
    /// let mut host = kevy_lua_host::LuaHost::<()>::new(|_, _argv, _ro| Vec::new());
    /// let sha = host.script_load(b"return 1");
    /// assert_eq!(sha, kevy_lua::sha1::sha1(b"return 1"));
    /// assert_eq!(host.script_exists(&[sha]), [true]);
    /// ```
    pub fn script_load(&mut self, script: &[u8]) -> ScriptSha1 {
        self.bridge.script_load(script)
    }

    /// SCRIPT EXISTS.
    ///
    /// ```
    /// let mut host = kevy_lua_host::LuaHost::<()>::new(|_, _argv, _ro| Vec::new());
    /// host.eval(&mut (), b"return 1", &[], &[]);
    /// let ran = kevy_lua::sha1::sha1(b"return 1");
    /// assert_eq!(host.script_exists(&[ran, [0; 20]]), [true, false]);
    /// ```
    #[must_use]
    pub fn script_exists(&self, sha1s: &[ScriptSha1]) -> Vec<bool> {
        self.bridge.script_exists(sha1s)
    }

    /// SCRIPT FLUSH.
    ///
    /// ```
    /// let mut host = kevy_lua_host::LuaHost::<()>::new(|_, _argv, _ro| Vec::new());
    /// let sha = host.script_load(b"return 1");
    /// host.script_flush(kevy_lua::FlushMode::Sync);
    /// assert_eq!(host.script_exists(&[sha]), [false]);
    /// ```
    pub fn script_flush(&mut self, mode: FlushMode) {
        self.bridge.script_flush(mode);
    }

    /// Forward [`kevy_lua::Bridge::set_instr_budget`] — set the
    /// per-Vm instruction cap. The operator wires `[lua]
    /// time_limit_ms` here at server startup.
    ///
    /// ```
    /// let mut host = kevy_lua_host::LuaHost::<()>::new(|_, _argv, _ro| Vec::new());
    /// host.set_instr_budget(1_000);
    /// assert!(host.eval(&mut (), b"while true do end", &[], &[]).starts_with(b"-"));
    /// ```
    pub fn set_instr_budget(&mut self, n: i64) {
        self.bridge.set_instr_budget(n);
    }

    /// Forward [`kevy_lua::Bridge::set_allowed_dialects`].
    ///
    /// ```
    /// use kevy_lua::LuaVersion;
    /// let mut host = kevy_lua_host::LuaHost::<()>::new(|_, _argv, _ro| Vec::new());
    /// host.set_allowed_dialects(&[LuaVersion::Lua51, LuaVersion::Lua54]);
    /// assert_eq!(host.eval(&mut (), b"#!lua version=5.4\nreturn 3 // 2", &[], &[]), b":1\r\n");
    /// assert!(host.eval(&mut (), b"#!lua version=5.3\nreturn 1", &[], &[]).starts_with(b"-ERR"));
    /// ```
    pub fn set_allowed_dialects(&mut self, versions: &[kevy_lua::LuaVersion]) {
        self.bridge.set_allowed_dialects(versions);
    }
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
