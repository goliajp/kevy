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
use luna_core::version::LuaVersion;
use luna_core::vm::exec::Vm;

mod host;
mod marshal;
mod resp;

/// Lua 5.1 / 5.2 / 5.3 / 5.4 / 5.5 — five fixed slots.
const N_DIALECTS: usize = 5;

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
}

impl Bridge {
    /// Create a fresh bridge. No VMs are spawned until the first
    /// EVAL.
    #[must_use]
    pub fn new() -> Self {
        Self {
            vms: [const { None }; N_DIALECTS],
        }
    }

    /// Restrict which Lua dialects this bridge will spawn VMs for.
    /// An EVAL with `#!lua version=N` for a non-allowed dialect is
    /// rejected with `-ERR dialect 5.X disabled by [lua]
    /// allow_dialects`.
    ///
    /// P1 stub — accepted but unused; enforcement lands in P4 when
    /// the shebang parser arrives.
    pub fn set_allowed_dialects(&mut self, _versions: &[LuaVersion]) {}

    /// Compile-or-execute a script and marshal its first return value
    /// into a RESP reply.
    ///
    /// P1 scope: default to Lua 5.1, ignore KEYS/ARGV (P3 binds them
    /// to globals), no `redis.call` (P3), no shebang parsing (P4),
    /// no SHA1 cache (P5). The point is to confirm
    /// `EVAL "return 1" 0` produces `:1\r\n`.
    pub fn eval(&mut self, script: &[u8], keys: &[&[u8]], args: &[&[u8]]) -> Reply {
        // P2+ will accept binary; for now, scripts must be UTF-8.
        let src = match std::str::from_utf8(script) {
            Ok(s) => s,
            Err(_) => return resp::err(b"script is not valid UTF-8"),
        };
        let vm = self.vm_for(LuaVersion::Lua51);
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

    /// Run a previously-cached script by SHA1 hex.
    ///
    /// P1 stub — cache lands in P5.
    pub fn evalsha(&mut self, _sha1: ScriptSha1, _keys: &[&[u8]], _args: &[&[u8]]) -> Reply {
        resp::err(b"NOSCRIPT No matching script. Please use EVAL.")
    }

    /// Cache a script without running it. Returns the SHA1.
    ///
    /// P1 stub — returns an all-zero SHA1.
    pub fn script_load(&mut self, _script: &[u8]) -> ScriptSha1 {
        [0u8; 20]
    }

    /// Test which of the given SHA1s are in the cache.
    ///
    /// P1 stub — returns all-false.
    #[must_use]
    pub fn script_exists(&self, sha1s: &[ScriptSha1]) -> Vec<bool> {
        vec![false; sha1s.len()]
    }

    /// Drop the SHA1 cache + all per-dialect VMs. Next EVAL spawns
    /// a fresh sandbox.
    pub fn script_flush(&mut self, _mode: FlushMode) {
        for slot in &mut self.vms {
            *slot = None;
        }
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
            let mut vm = Vm::sandbox(version)
                .open_base()
                .open_math()
                .open_string()
                .open_table()
                .with_instr_budget(200_000_000)
                .build();
            host::install_redis_table(&mut vm);
            *slot = Some(vm);
        }
        slot.as_mut().expect("just-inserted Vm")
    }
}

impl Default for Bridge {
    fn default() -> Self {
        Self::new()
    }
}

fn format_lua_error(e: &luna_core::vm::error::LuaError) -> String {
    // luna v1.1 B6: `impl Display for LuaError` — embedders no longer
    // need to case-split on the inner Value type.
    format!("{e}")
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
        let mut b = Bridge::new();
        assert_eq!(b.eval(b"return 1", &[], &[]), b":1\r\n");
        assert_eq!(b.eval(b"return 2", &[], &[]), b":2\r\n");
        // One VM should be cached for the 5.1 default dialect.
        assert_eq!(b.vm_count(), 1);
    }

    #[test]
    fn script_flush_drops_vm_pool() {
        let mut b = Bridge::new();
        let _ = b.eval(b"return 1", &[], &[]);
        assert_eq!(b.vm_count(), 1);
        b.script_flush(FlushMode::Sync);
        assert_eq!(b.vm_count(), 0);
    }
}
