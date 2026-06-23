//! kevy-lua — Redis EVAL / EVALSHA / SCRIPT surface backed by luna-core.
//!
//! kevy's v1.27 script-host layer. Thin "cement" crate (per the
//! stone-cement-stone model) — it carries no algorithmic content, only
//! the bridge between kevy-rt's command dispatch path, kevy-resp's
//! wire codec, and luna-core's sandboxed `Vm`.
//!
//! Design lock-in (see `.claude/rfcs/2026-06-23-v1.27-luna-bridge.md`):
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
//! # Phase status — P1
//!
//! - `Bridge` holds a per-dialect Vm pool (lazy-spawned).
//! - `eval()` runs the script under the default 5.1 sandbox and
//!   marshals the first returned `Value` into a RESP reply.
//! - Shebang parsing, SHA1 cache, EVALSHA, SCRIPT LOAD/EXISTS/FLUSH,
//!   and `redis.call` host plumbing land in P2-P5 per the RFC.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use luna_core::runtime::value::Value;
use luna_core::vm::exec::Vm;
use std::cell::Cell;
use std::rc::Rc;

mod dispatch;
mod host;
mod marshal;
mod resp;
mod shebang;

/// SHA-1 digest helpers. Exposed because the operator-side wire
/// layer (kevy-rt's SCRIPT LOAD / EVALSHA codec) needs to convert
/// between the 20-byte digest used as a cache key and the 40-char
/// ASCII hex Redis uses on the wire.
pub mod sha1;

/// Re-export so callers can name the dialect without depending on
/// luna-core directly.
pub use luna_core::version::LuaVersion;

pub(crate) use dispatch::{DispatchHandle, DispatchSlot, DISPATCH_KEY};

/// Lua 5.1 / 5.2 / 5.3 / 5.4 / 5.5 — five fixed slots.
const N_DIALECTS: usize = 5;

/// 200 M ≈ 5 s on modern hardware; matches Redis's default
/// `lua-time-limit`. Overridable via [`Bridge::set_instr_budget`].
/// `0` = unlimited (no budget cap).
const DEFAULT_INSTR_BUDGET: i64 = 200_000_000;

fn dialect_slot(v: LuaVersion) -> usize {
    // `LuaVersion` is a `#[repr(...)]` C-style enum with the variants
    // in version order (Lua51 = 0, …, Lua55 = 4). Stable per luna's
    // semver promise (the variant declaration order is the wire layout
    // — luna's docs explicitly say "New variants must be appended").
    v as usize
}

/// A wire-level reply: just the encoded RESP bytes.
pub type Reply = Vec<u8>;

/// SCRIPT FLUSH mode (Redis 6.2+ semantics).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlushMode {
    /// Synchronous — drop the cache before returning.
    Sync,
    /// Asynchronous — schedule the cache drop. v1.27 implements
    /// both modes as Sync; we keep the tag for future
    /// differentiation (and Redis-compat replies).
    Async,
}

/// A SHA1 hash of a script's source bytes. Used as the EVALSHA cache
/// key. Includes any `#!lua version=N` shebang in the input, so a
/// 5.1 script and the same script with a 5.3 shebang have distinct
/// SHA1s and never collide in the cache.
pub type ScriptSha1 = [u8; 20];

/// kevy-lua per-shard bridge. One `Bridge` lives in each shard's
/// runtime; it owns the per-dialect VM pool, the SHA1 cache, and
/// (P3+) the kevy-side dispatch callback that `redis.call` invokes.
///
/// The bridge is intentionally NOT `Send` / `Sync` — same constraint
/// as luna's `Vm`, which is `!Send + !Sync` by design. kevy's
/// thread-per-core model means every shard owns its bridge
/// exclusively.
pub struct Bridge {
    /// Lazily-spawned VM per dialect. First EVAL hitting a dialect
    /// creates the VM; reused for every subsequent script on that
    /// dialect. Five fixed slots indexed by [`dialect_slot`] (luna
    /// `LuaVersion` is a 0-4 discriminant in version order).
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
    /// (`Vm::sandbox(...).with_instr_budget(N)`). Default 200 M,
    /// matches the v1.27 P1-P6 hard-coded behaviour. The kevy
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
    pub fn set_instr_budget(&mut self, n: i64) {
        self.instr_budget = n;
    }

    /// Bridge with a no-op dispatcher: every `redis.call` returns a
    /// RESP error. Convenience for embedders that want EVAL but
    /// don't have the host dispatch wired yet (e.g. pure-computation
    /// scripts during early development).
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

    /// Compile-or-execute a script and marshal its first return value
    /// into a RESP reply.
    ///
    /// P1 scope: default to Lua 5.1, ignore KEYS/ARGV (P3 binds them
    /// to globals), no `redis.call` (P3), no shebang parsing (P4),
    /// no SHA1 cache (P5). The point is to confirm
    /// `EVAL "return 1" 0` produces `:1\r\n`.
    pub fn eval(&mut self, script: &[u8], keys: &[&[u8]], args: &[&[u8]]) -> Reply {
        // P4: peel off the `#!lua version=N` shebang first so we know
        // which dialect Vm to route to before parsing the body.
        let (sh, body) = match shebang::parse(script) {
            Ok(t) => t,
            Err(e) => return resp::err(format!("{e}").as_bytes()),
        };
        if !self.allow[dialect_slot(sh.version)] {
            return resp::err(
                format!(
                    "dialect {} disabled by [lua] allow_dialects",
                    version_tag(sh.version)
                )
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
    pub fn eval_ro(&mut self, script: &[u8], keys: &[&[u8]], args: &[&[u8]]) -> Reply {
        self.read_only.set(true);
        let r = self.eval(script, keys, args);
        self.read_only.set(false);
        r
    }

    /// Read-only variant of [`Bridge::evalsha`]. Redis 7.0+ `EVALSHA_RO`.
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
    pub fn evalsha(&mut self, sha1: ScriptSha1, keys: &[&[u8]], args: &[&[u8]]) -> Reply {
        let Some(script) = self.script_cache.get(&sha1).cloned() else {
            return resp::err(b"NOSCRIPT No matching script. Please use EVAL.");
        };
        self.eval(&script, keys, args)
    }

    /// Cache a script without running it. Returns the SHA1 digest;
    /// the operator-side wire layer hex-encodes it for the Redis
    /// SCRIPT LOAD reply.
    pub fn script_load(&mut self, script: &[u8]) -> ScriptSha1 {
        let digest = sha1::sha1(script);
        self.script_cache.insert(digest, script.to_vec());
        digest
    }

    /// Test which of the given SHA1s are in the cache. Returns a
    /// vector with `true`/`false` for each input SHA1 in order.
    #[must_use]
    pub fn script_exists(&self, sha1s: &[ScriptSha1]) -> Vec<bool> {
        sha1s.iter().map(|s| self.script_cache.contains_key(s)).collect()
    }

    /// Drop the SHA1 cache + all per-dialect VMs. `ASYNC` and `SYNC`
    /// are currently both implemented as synchronous; the tag is
    /// preserved for future differentiation.
    pub fn script_flush(&mut self, _mode: FlushMode) {
        for slot in &mut self.vms {
            *slot = None;
        }
        self.script_cache.clear();
    }


    /// Number of dialect VMs currently spawned. Test-only helper —
    /// production code doesn't need to inspect the pool.
    #[cfg(test)]
    fn vm_count(&self) -> usize {
        self.vms.iter().filter(|s| s.is_some()).count()
    }

    /// Lazily build the sandbox Vm for `version`. Conservative
    /// default: base + math + string + table libraries, no JIT,
    /// no bytecode loading, 200M instruction budget (~5 s on modern
    /// hardware — Redis's default `lua-time-limit`). The `redis`
    /// host table is installed once at Vm-construction time; KEYS /
    /// ARGV are re-bound per `eval` call (see [`Bridge::eval`]).
    fn vm_for(&mut self, version: LuaVersion) -> &mut Vm {
        let slot = &mut self.vms[dialect_slot(version)];
        if slot.is_none() {
            let mut builder = Vm::sandbox(version)
                .open_base()
                .open_math()
                .open_string()
                .open_table();
            if self.instr_budget > 0 {
                builder = builder.with_instr_budget(self.instr_budget);
            }
            let mut vm = builder.build();
            host::install_redis_table(&mut vm);
            // Install the dispatch handle as a luna userdata global
            // (luna v1.1 B8). `redis.call` retrieves it via
            // `vm.userdata_borrow::<DispatchSlot>(DISPATCH_KEY)`. We
            // clone the Rc so each Vm holds an independent handle
            // pointing at the shared closure.
            let _ = vm.set_userdata(
                DISPATCH_KEY,
                DispatchSlot {
                    f: Rc::clone(&self.dispatch),
                    read_only: Rc::clone(&self.read_only),
                },
            );
            *slot = Some(vm);
        }
        slot.as_mut().expect("just-inserted Vm")
    }
}

impl Default for Bridge {
    /// Equivalent to [`Bridge::with_no_dispatch`] — the safe default
    /// for embedders that don't have a host dispatch wired yet.
    fn default() -> Self {
        Self::with_no_dispatch()
    }
}

fn format_lua_error(e: &luna_core::vm::error::LuaError) -> String {
    // luna v1.1 B6: `impl Display for LuaError` — embedders no longer
    // need to case-split on the inner Value type.
    format!("{e}")
}

fn version_tag(v: LuaVersion) -> &'static str {
    match v {
        LuaVersion::Lua51 => "5.1",
        LuaVersion::Lua52 => "5.2",
        LuaVersion::Lua53 => "5.3",
        LuaVersion::Lua54 => "5.4",
        LuaVersion::Lua55 => "5.5",
    }
}


// Most public-surface tests live in `tests/integration.rs` (the
// house-rule 500 LOC limit on src/*.rs is preserved that way). The
// few unit tests below need `Bridge::vm_count`, which is
// `#[cfg(test)]`-gated and therefore not visible from
// integration tests.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eval_reuses_vm_across_calls() {
        let mut b = Bridge::with_no_dispatch();
        assert_eq!(b.eval(b"return 1", &[], &[]), b":1\r\n");
        assert_eq!(b.eval(b"return 2", &[], &[]), b":2\r\n");
        // One VM should be cached for the 5.1 default dialect.
        assert_eq!(b.vm_count(), 1);
    }

    #[test]
    fn script_flush_drops_vm_pool() {
        let mut b = Bridge::with_no_dispatch();
        let _ = b.eval(b"return 1", &[], &[]);
        assert_eq!(b.vm_count(), 1);
        b.script_flush(FlushMode::Sync);
        assert_eq!(b.vm_count(), 0);
    }
}
