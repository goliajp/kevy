//! kevy-lua — Redis EVAL / EVALSHA / SCRIPT surface backed by luna-core.
//!
//! kevy's script-host layer. Thin "cement" crate (per the
//! stone-cement-stone model) — it carries no algorithmic content, only
//! the bridge between kevy-rt's command dispatch path, kevy-resp's
//! wire codec, and luna-core's sandboxed `Vm`.
//!
//! Design lock-in:
//!
//! - **Default Lua 5.1** — preserves the Redis Lua ecosystem (BullMQ,
//!   Redlock, rate limiters, anything copied from Redis docs).
//! - **Per-script dialect opt-in via `#!lua version=N`** — scripts
//!   opt into 5.2 / 5.3 / 5.4 / 5.5 with a single shebang line.
//!   SHA1 cache key is the raw script bytes, so EVALSHA is
//!   version-aware for free.
//! - **VM per-shard, per-dialect, lazily spawned** — first EVAL
//!   hitting a dialect on a shard constructs the VM; reused
//!   afterwards. Idle RSS scales with dialects actually used.
//! - **Atomic execution** — entering EVAL pauses other dispatch on
//!   that shard until the script returns. Matches Redis semantics.
//!
//! `Bridge` holds a per-dialect Vm pool (lazy-spawned); `eval()` runs
//! the script under the sandbox and marshals the first returned
//! `Value` into a RESP reply. Shebang parsing, SHA1 cache, EVALSHA,
//! SCRIPT LOAD/EXISTS/FLUSH, and the `redis.call` host plumbing all
//! live here.
//!
//! ```
//! let mut bridge = kevy_lua::Bridge::with_no_dispatch();
//! // KEYS and ARGV are bound per call; the reply is RESP bytes.
//! let reply = bridge.eval(b"return #KEYS + tonumber(ARGV[1])", &[b"k"], &[b"41"]);
//! assert_eq!(reply, b":42\r\n");
//! // EVAL caches the script, so EVALSHA finds it by digest.
//! let sha = kevy_lua::sha1::sha1(b"return 'hi'");
//! bridge.eval(b"return 'hi'", &[], &[]);
//! assert_eq!(bridge.script_exists(&[sha]), vec![true]);
//! ```

// Seeding a sandbox global. A VM that refuses one fails the script
// anyway, with its own message — "attempt to index a nil value" says
// more than "set_global returned Err".
#![expect(
    clippy::let_underscore_must_use,
    reason = "a VM that refuses a global fails the script with a better message"
)]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use luna_core::runtime::value::Value;
use luna_core::vm::exec::Vm;
use std::cell::Cell;
use std::rc::Rc;

mod dispatch;
mod host;
mod marshal;
mod pool;
mod resp;
mod shebang;

mod cjson;
mod cmsgpack;
/// SHA-1 digest helpers. Exposed because the operator-side wire
/// layer (kevy-rt's SCRIPT LOAD / EVALSHA codec) needs to convert
/// between the 20-byte digest used as a cache key and the 40-char
/// ASCII hex Redis uses on the wire.
pub mod sha1;

/// Re-export so callers can name the dialect without depending on
/// luna-core directly.
pub use luna_core::version::LuaVersion;

pub(crate) use dispatch::{DISPATCH_KEY, DispatchHandle, DispatchSlot};
use pool::{dialect_slot, format_lua_error, version_tag};

/// Lua 5.1 / 5.2 / 5.3 / 5.4 / MacroLua / 5.5 — six fixed slots.
/// `MacroLua` sits between `Lua54` and `Lua55` (it's a 5.4-superset
/// compile-time-macro dialect). luna-core does not promise
/// append-only variant ordering, so we explicitly map every variant
/// to a stable slot via [`dialect_slot`] — an upstream variant
/// insert can't silently re-index the VM pool.
const N_DIALECTS: usize = 6;

/// 200 M ≈ 5 s on modern hardware; matches Redis's default
/// `lua-time-limit`. Overridable via [`Bridge::set_instr_budget`].
/// `0` = unlimited (no budget cap).
const DEFAULT_INSTR_BUDGET: i64 = 200_000_000;

/// A wire-level reply: just the encoded RESP bytes.
///
/// ```
/// let reply: kevy_lua::Reply = kevy_lua::Bridge::with_no_dispatch().eval(b"return 'ok'", &[], &[]);
/// assert_eq!(reply, b"$2\r\nok\r\n");
/// ```
pub type Reply = Vec<u8>;

/// SCRIPT FLUSH mode (Redis 6.2+ semantics). The default is
/// [`FlushMode::Sync`], what `SCRIPT FLUSH` without an argument means.
///
/// ```
/// assert_eq!(kevy_lua::FlushMode::default(), kevy_lua::FlushMode::Sync);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum FlushMode {
    /// Synchronous — drop the cache before returning.
    ///
    /// ```
    /// let mut b = kevy_lua::Bridge::with_no_dispatch();
    /// let sha = b.script_load(b"return 1");
    /// b.script_flush(kevy_lua::FlushMode::Sync);
    /// assert_eq!(b.script_exists(&[sha]), [false]);
    /// ```
    #[default]
    Sync,
    /// Asynchronous — schedule the cache drop. Currently both
    /// modes run as Sync; we keep the tag for future
    /// differentiation (and Redis-compat replies).
    ///
    /// ```
    /// let mut b = kevy_lua::Bridge::with_no_dispatch();
    /// let sha = b.script_load(b"return 1");
    /// b.script_flush(kevy_lua::FlushMode::Async);
    /// // the cache is already empty when the call returns
    /// assert_eq!(b.script_exists(&[sha]), [false]);
    /// ```
    Async,
}

/// A SHA1 hash of a script's source bytes. Used as the EVALSHA cache
/// key. Includes any `#!lua version=N` shebang in the input, so a
/// 5.1 script and the same script with a 5.3 shebang have distinct
/// SHA1s and never collide in the cache.
///
/// ```
/// use kevy_lua::{Bridge, ScriptSha1};
/// let mut b = Bridge::with_no_dispatch();
/// let plain: ScriptSha1 = b.script_load(b"return 1");
/// let pinned: ScriptSha1 = b.script_load(b"#!lua version=5.3\nreturn 1");
/// assert_eq!(plain, kevy_lua::sha1::sha1(b"return 1"));
/// assert_ne!(plain, pinned);
/// ```
pub type ScriptSha1 = [u8; 20];

/// kevy-lua per-shard bridge. One `Bridge` lives in each shard's
/// runtime; it owns the per-dialect VM pool, the SHA1 cache, and
/// the kevy-side dispatch callback that `redis.call` invokes.
///
/// The bridge is intentionally NOT `Send` / `Sync` — same constraint
/// as luna-core's `Vm`, which is `!Send + !Sync` by design. kevy's
/// thread-per-core model means every shard owns its bridge
/// exclusively.
///
/// ```
/// let mut bridge = kevy_lua::Bridge::with_no_dispatch();
/// // no shebang runs under Lua 5.1; `#!lua version=5.3` opts into 5.3
/// assert_eq!(bridge.eval(b"return 7 / 2", &[], &[]), b"$3\r\n3.5\r\n");
/// assert_eq!(bridge.eval(b"#!lua version=5.3\nreturn 7 // 2", &[], &[]), b":3\r\n");
/// ```
pub struct Bridge {
    /// Lazily-spawned VM per dialect. First EVAL hitting a dialect
    /// creates the VM; reused for every subsequent script on that
    /// dialect. Six fixed slots indexed by [`dialect_slot`].
    vms: [Option<Vm>; N_DIALECTS],
    /// Host dispatch closure invoked by `redis.call` / `redis.pcall`.
    /// `Rc` so cheaply cloned into per-Vm userdata at construction
    /// time without consuming the original.
    dispatch: DispatchHandle,
    /// Read-only mode flag set by [`Bridge::eval_ro`] /
    /// [`Bridge::evalsha_ro`] before running the script and cleared
    /// right after. `Rc<Cell<...>>` so every per-dialect Vm's
    /// dispatch userdata sees the same bit without us having to
    /// walk the pool. Shared with each `DispatchSlot`.
    read_only: Rc<Cell<bool>>,
    /// Per-Vm instruction budget applied at construction time
    /// (`Vm::sandbox(...).with_instr_budget(N)`). Default 200 M
    /// (the original hard-coded value). The kevy
    /// operator wires `[lua] time_limit_ms` through here via
    /// [`Bridge::set_instr_budget`].
    ///
    /// Changes only affect VMs spawned **after** the setter call;
    /// the kevy-side wiring sets it before any EVAL so this is fine
    /// in practice. If a config reload needs to take effect on
    /// in-flight VMs, call `script_flush` afterwards.
    instr_budget: i64,
    /// Allow-mask, one bit per [`dialect_slot`]. `true` at slot `i`
    /// means dialect `i` is permitted; an EVAL whose shebang asks
    /// for a denied dialect gets a wire `-ERR` reply. All-true by
    /// default.
    allow: [bool; N_DIALECTS],
    /// SHA1 → raw script bytes (including shebang). Populated by
    /// `script_load` and by every successful `eval`. EVALSHA reads
    /// from here; SCRIPT FLUSH empties it; SCRIPT EXISTS probes it.
    ///
    /// Per-shard cache: kevy runs thread-per-core and each shard
    /// owns its own Bridge, so we don't share a global cache. The
    /// trade-off (cache miss on first hit per shard) is dwarfed by
    /// the locking we'd otherwise need.
    script_cache: std::collections::HashMap<ScriptSha1, Vec<u8>>,
}

impl Bridge {
    /// Create a fresh bridge with `dispatch` as the host callback
    /// behind `redis.call`. No Vms are spawned until the first
    /// EVAL.
    ///
    /// The dispatch closure receives the script's argv (`&[&[u8]]`,
    /// command name at index 0) plus a `read_only` flag and must
    /// return RESP reply bytes. When `read_only` is true the
    /// dispatcher MUST reject write commands with
    /// `-READONLY can't write against a read-only script\r\n` so
    /// `EVAL_RO` / `EVALSHA_RO` deliver Redis semantics. kevy-rt
    /// owns the canonical command-flag table and does this check
    /// natively in production; tests provide a stub dispatcher
    /// hard-coding a few write commands (see `tests/integration.rs`).
    ///
    /// For embedders that don't need real keyspace access (e.g.
    /// pure-computation EVAL), [`Bridge::with_no_dispatch`] installs
    /// a default that returns `-ERR redis.call: no dispatch wired`
    /// for every call.
    ///
    /// ```
    /// // a host that answers every command with its own name
    /// let mut b = kevy_lua::Bridge::new(|argv: &[&[u8]], _read_only: bool| {
    ///     let name = argv[0];
    ///     [format!("${}\r\n", name.len()).as_bytes(), name, b"\r\n"].concat()
    /// });
    /// assert_eq!(b.eval(b"return redis.call('ECHO')", &[], &[]), b"$4\r\nECHO\r\n");
    /// ```
    pub fn new<F>(dispatch: F) -> Self
    where
        F: Fn(&[&[u8]], bool) -> Vec<u8> + 'static,
    {
        Self {
            vms: [const { None }; N_DIALECTS],
            dispatch: Rc::new(dispatch),
            read_only: Rc::new(Cell::new(false)),
            allow: [true; N_DIALECTS],
            script_cache: std::collections::HashMap::new(),
            instr_budget: DEFAULT_INSTR_BUDGET,
        }
    }

    /// Override the per-Vm instruction budget (~5 s ≈ 200 M instr by
    /// default). `0` disables the cap (unlimited execution).
    ///
    /// Setting it does NOT affect already-spawned VMs in the pool —
    /// you can pair the call with [`Bridge::script_flush`] to force
    /// a respawn under the new budget, or leave existing VMs as-is
    /// and only catch new dialects.
    ///
    /// ```
    /// let mut b = kevy_lua::Bridge::with_no_dispatch();
    /// b.set_instr_budget(10_000);
    /// // a runaway script is stopped and reported, not left spinning
    /// assert!(b.eval(b"while true do end", &[], &[]).starts_with(b"-"));
    /// ```
    pub fn set_instr_budget(&mut self, n: i64) {
        self.instr_budget = n;
    }

    /// Bridge with a no-op dispatcher: every `redis.call` returns a
    /// RESP error. Convenience for embedders that want EVAL but
    /// don't have the host dispatch wired yet (e.g. pure-computation
    /// scripts during early development).
    ///
    /// ```
    /// let mut b = kevy_lua::Bridge::with_no_dispatch();
    /// assert_eq!(b.eval(b"return 6 * 7", &[], &[]), b":42\r\n");
    /// assert!(b.eval(b"return redis.call('GET', 'k')", &[], &[]).starts_with(b"-"));
    /// ```
    #[must_use]
    pub fn with_no_dispatch() -> Self {
        Self::new(|_argv: &[&[u8]], _ro: bool| {
            b"-ERR redis.call: no host dispatch wired\r\n".to_vec()
        })
    }

    /// Restrict which Lua dialects this bridge will spawn VMs for.
    /// An EVAL with `#!lua version=N` for a non-allowed dialect is
    /// rejected with a `-ERR` reply. The 5.1 default is always
    /// accessible via scripts with no shebang regardless of this
    /// setting (you can't disable the ecosystem-default dialect
    /// without taking a different `with_allowed_dialects` API).
    ///
    /// Passing an empty slice = no restriction = all five dialects
    /// permitted (the constructor default).
    ///
    /// ```
    /// use kevy_lua::{Bridge, LuaVersion};
    /// let mut b = Bridge::with_no_dispatch();
    /// b.set_allowed_dialects(&[LuaVersion::Lua51]);
    /// assert!(b.eval(b"#!lua version=5.4\nreturn 1", &[], &[]).starts_with(b"-ERR"));
    /// assert_eq!(b.eval(b"return 1", &[], &[]), b":1\r\n");
    /// ```
    pub fn set_allowed_dialects(&mut self, versions: &[LuaVersion]) {
        if versions.is_empty() {
            self.allow = [true; N_DIALECTS];
            return;
        }
        self.allow = [false; N_DIALECTS];
        for v in versions {
            self.allow[dialect_slot(*v)] = true;
        }
    }

    /// Run a script and marshal its first return value into a RESP
    /// reply: the `#!lua version=N` shebang picks the dialect (5.1 when
    /// absent), `KEYS` and `ARGV` are bound for this call, and the script
    /// enters the SHA1 cache before it runs, as Redis does. Every failure
    /// — a bad shebang, a disabled dialect, a Lua error — is a RESP error
    /// reply, never a panic.
    ///
    /// ```
    /// let mut b = kevy_lua::Bridge::with_no_dispatch();
    /// assert_eq!(b.eval(b"return 1", &[], &[]), b":1\r\n");
    /// assert!(b.eval(b"error('boom')", &[], &[]).starts_with(b"-"));
    /// ```
    pub fn eval(&mut self, script: &[u8], keys: &[&[u8]], args: &[&[u8]]) -> Reply {
        // P4: peel off the `#!lua version=N` shebang first so we know
        // which dialect Vm to route to before parsing the body.
        let (sh, body) = match shebang::parse(script) {
            Ok(t) => t,
            Err(e) => return resp::err(format!("{e}").as_bytes()),
        };
        if !self.allow[dialect_slot(sh.version)] {
            return resp::err(
                format!("dialect {} disabled by [lua] allow_dialects", version_tag(sh.version))
                    .as_bytes(),
            );
        }
        let src = match std::str::from_utf8(body) {
            Ok(s) => s,
            Err(_) => return resp::err(b"script body is not valid UTF-8"),
        };
        // Redis EVAL semantics: every script that successfully runs
        // (or even compiles) is added to the SCRIPT cache so a later
        // EVALSHA can find it. We insert before running so a script
        // that runs forever still gets a SCRIPT EXISTS hit (matches
        // Redis behaviour).
        let digest = sha1::sha1(script);
        self.script_cache.entry(digest).or_insert_with(|| script.to_vec());
        let vm = self.vm_for(sh.version);
        // Bind KEYS / ARGV freshly per invocation. The `redis` host
        // table was installed once when the Vm was constructed.
        host::bind_keys_argv(vm, keys, args);
        match vm.eval(src) {
            Ok(results) => {
                let first = results.first().copied().unwrap_or(Value::Nil);
                marshal::value(vm, first)
            }
            Err(e) => resp::err(format_lua_error(&e).as_bytes()),
        }
    }

    /// Read-only variant of [`Bridge::eval`]. The dispatcher receives
    /// `read_only = true` for every `redis.call` from this script;
    /// kevy-rt rejects write commands with
    /// `-READONLY can't write against a read-only script\r\n`.
    /// Redis 7.0+ `EVAL_RO`.
    ///
    /// All other semantics (KEYS / ARGV / SHA1 cache fill /
    /// dialect routing) are identical to `eval`.
    ///
    /// ```
    /// // the host sees the read-only flag and refuses writes under it
    /// let mut b = kevy_lua::Bridge::new(|argv: &[&[u8]], read_only: bool| {
    ///     if read_only && argv[0] == b"SET" {
    ///         b"-READONLY can't write against a read-only script\r\n".to_vec()
    ///     } else {
    ///         b"+OK\r\n".to_vec()
    ///     }
    /// });
    /// let script = b"return redis.pcall('SET', KEYS[1], 'v')";
    /// assert_eq!(b.eval(script, &[b"k"], &[]), b"+OK\r\n");
    /// assert!(b.eval_ro(script, &[b"k"], &[]).starts_with(b"-READONLY"));
    /// ```
    pub fn eval_ro(&mut self, script: &[u8], keys: &[&[u8]], args: &[&[u8]]) -> Reply {
        self.read_only.set(true);
        let r = self.eval(script, keys, args);
        self.read_only.set(false);
        r
    }

    /// Read-only variant of [`Bridge::evalsha`]. Redis 7.0+ `EVALSHA_RO`.
    ///
    /// ```
    /// let mut b = kevy_lua::Bridge::new(|_argv: &[&[u8]], read_only: bool| {
    ///     if read_only { b":1\r\n".to_vec() } else { b":0\r\n".to_vec() }
    /// });
    /// let sha = b.script_load(b"return redis.call('PING')");
    /// assert_eq!(b.evalsha_ro(sha, &[], &[]), b":1\r\n");
    /// assert_eq!(b.evalsha(sha, &[], &[]), b":0\r\n");
    /// ```
    pub fn evalsha_ro(&mut self, sha1: ScriptSha1, keys: &[&[u8]], args: &[&[u8]]) -> Reply {
        self.read_only.set(true);
        let r = self.evalsha(sha1, keys, args);
        self.read_only.set(false);
        r
    }

    /// Run a previously-cached script by SHA1 hex.
    ///
    /// Returns `-NOSCRIPT ...` if the script isn't in the cache.
    /// Identical to running `eval` with the cached bytes — the same
    /// shebang routing, KEYS/ARGV binding, and redis.* host plumbing
    /// apply.
    ///
    /// ```
    /// let mut b = kevy_lua::Bridge::with_no_dispatch();
    /// let sha = b.script_load(b"return ARGV[1]");
    /// assert_eq!(b.evalsha(sha, &[], &[b"hi"]), b"$2\r\nhi\r\n");
    /// assert!(b.evalsha([0; 20], &[], &[]).starts_with(b"-NOSCRIPT"));
    /// ```
    pub fn evalsha(&mut self, sha1: ScriptSha1, keys: &[&[u8]], args: &[&[u8]]) -> Reply {
        let Some(script) = self.script_cache.get(&sha1).cloned() else {
            return resp::err(b"NOSCRIPT No matching script. Please use EVAL.");
        };
        self.eval(&script, keys, args)
    }

    /// Cache a script without running it. Returns the SHA1 digest;
    /// the operator-side wire layer hex-encodes it for the Redis
    /// SCRIPT LOAD reply.
    ///
    /// ```
    /// let mut b = kevy_lua::Bridge::with_no_dispatch();
    /// let sha = b.script_load(b"return 1");
    /// assert_eq!(&kevy_lua::sha1::hex(&sha), b"e0e1f9fabfc9d4800c877a703b823ac0578ff8db");
    /// ```
    pub fn script_load(&mut self, script: &[u8]) -> ScriptSha1 {
        let digest = sha1::sha1(script);
        self.script_cache.insert(digest, script.to_vec());
        digest
    }

    /// Test which of the given SHA1s are in the cache. Returns a
    /// vector with `true`/`false` for each input SHA1 in order.
    ///
    /// ```
    /// let mut b = kevy_lua::Bridge::with_no_dispatch();
    /// let loaded = b.script_load(b"return 1");
    /// assert_eq!(b.script_exists(&[loaded, [0; 20]]), [true, false]);
    /// ```
    #[must_use]
    pub fn script_exists(&self, sha1s: &[ScriptSha1]) -> Vec<bool> {
        sha1s.iter().map(|s| self.script_cache.contains_key(s)).collect()
    }

    /// Drop the SHA1 cache + all per-dialect VMs. `ASYNC` and `SYNC`
    /// are currently both implemented as synchronous; the tag is
    /// preserved for future differentiation.
    ///
    /// ```
    /// let mut b = kevy_lua::Bridge::with_no_dispatch();
    /// let sha = b.script_load(b"return 1");
    /// b.script_flush(kevy_lua::FlushMode::default());
    /// assert!(b.evalsha(sha, &[], &[]).starts_with(b"-NOSCRIPT"));
    /// ```
    pub fn script_flush(&mut self, _mode: FlushMode) {
        for slot in &mut self.vms {
            *slot = None;
        }
        self.script_cache.clear();
    }
}

impl Default for Bridge {
    /// Equivalent to [`Bridge::with_no_dispatch`] — the safe default
    /// for embedders that don't have a host dispatch wired yet.
    fn default() -> Self {
        Self::with_no_dispatch()
    }
}

// Send and Sync are part of the public contract: a change that loses
// either fails to compile here rather than in a caller.
const _: () = {
    const fn send_sync<T: Send + Sync>() {}
    send_sync::<FlushMode>();
};
