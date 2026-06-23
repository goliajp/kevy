//! kevy-lua — Redis EVAL / EVALSHA / SCRIPT surface backed by luna-core.
//!
//! kevy's v1.27 script-host layer. Thin "cement" crate (per the
//! stone-cement-stone model in `methodology/steel-cement-stone.md`) —
//! it carries no algorithmic content, only the bridge between
//! kevy-rt's command dispatch path, kevy-resp's wire codec, and
//! luna-core's sandboxed `Vm`.
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
//! # P0 scope (this file's current state)
//!
//! Skeleton only. The public API surface is stubbed; calling any
//! method returns a placeholder error until P1 lands the real
//! implementation. P0 ships:
//!
//! - Workspace member registration (`crates/kevy-lua` in root
//!   `Cargo.toml`).
//! - `luna-core` exemption documented in workspace lockdown comment.
//! - Public API shape ≤ 10 functions (per CLAUDE.md house rule).
//! - Compile + one passing smoke test that exercises the dep wiring.
//!
//! P1+ adds the actual EVAL plumbing — see the RFC.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use luna_core::version::LuaVersion;

/// A wire-level reply: just the encoded RESP bytes. The bridge
/// hands these to kevy-rt's dispatch path, same as every other
/// kevy command.
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
/// the kevy-side dispatch callback that `redis.call` invokes.
///
/// The bridge is intentionally NOT `Send` / `Sync` — same constraint
/// as luna's `Vm`, which is `!Send + !Sync` by design. kevy's
/// thread-per-core model means every shard owns its bridge
/// exclusively.
pub struct Bridge {
    /// Reserved for P1+ — luna VMs keyed by dialect.
    _placeholder: core::marker::PhantomData<*const ()>,
}

impl Bridge {
    /// Create a fresh bridge with the conservative-default sandbox
    /// (whitelisted stdlib: base + math + string + table; JIT off;
    /// bytecode loading off). Configure further with the builder
    /// methods.
    ///
    /// P0 stub — returns an empty bridge that rejects every command
    /// until P1 wires the real Vm pool.
    #[must_use]
    pub fn new() -> Self {
        Self {
            _placeholder: core::marker::PhantomData,
        }
    }

    /// Restrict which Lua dialects this bridge will spawn VMs for.
    /// An EVAL with `#!lua version=N` for a non-allowed dialect is
    /// rejected with `-ERR dialect 5.X disabled by [lua]
    /// allow_dialects`.
    ///
    /// Empty slice = no restriction = all five dialects allowed
    /// (the default).
    ///
    /// P0 stub — accepted but unused.
    pub fn set_allowed_dialects(&mut self, _versions: &[LuaVersion]) {
        // P1: persist the allow-list + filter shebang lookups.
    }

    /// Compile-or-cache a script by SHA1, execute it, marshal the
    /// reply.
    ///
    /// P0 stub — returns the placeholder reply
    /// `-ERR kevy-lua P0 stub`.
    pub fn eval(&mut self, _script: &[u8], _keys: &[&[u8]], _args: &[&[u8]]) -> Reply {
        stub_reply(b"kevy-lua P0 stub")
    }

    /// Run a previously-cached script by SHA1 hex.
    ///
    /// P0 stub — same placeholder reply.
    pub fn evalsha(&mut self, _sha1: ScriptSha1, _keys: &[&[u8]], _args: &[&[u8]]) -> Reply {
        stub_reply(b"kevy-lua P0 stub")
    }

    /// Cache a script without running it. Returns the SHA1.
    ///
    /// P0 stub — returns an all-zero SHA1.
    pub fn script_load(&mut self, _script: &[u8]) -> ScriptSha1 {
        [0u8; 20]
    }

    /// Test which of the given SHA1s are in the cache.
    ///
    /// P0 stub — returns all-false.
    #[must_use]
    pub fn script_exists(&self, sha1s: &[ScriptSha1]) -> Vec<bool> {
        vec![false; sha1s.len()]
    }

    /// Drop the SHA1 cache.
    pub fn script_flush(&mut self, _mode: FlushMode) {
        // P1: clear per-dialect caches.
    }
}

impl Default for Bridge {
    fn default() -> Self {
        Self::new()
    }
}

fn stub_reply(msg: &[u8]) -> Reply {
    // RESP error reply: `-ERR <msg>\r\n`. Hand-rolled (no kevy-resp
    // encoder dep needed at P0) to keep the stub minimal.
    let mut out = Vec::with_capacity(msg.len() + 7);
    out.push(b'-');
    out.extend_from_slice(b"ERR ");
    out.extend_from_slice(msg);
    out.extend_from_slice(b"\r\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P0 smoke — bridge constructs and the stub eval path
    /// produces a syntactically-valid RESP error reply. Confirms
    /// luna-core wired into the build graph (the `use luna_core::*`
    /// at the top of the file fails to compile if the dep is
    /// misconfigured).
    #[test]
    fn p0_skeleton_compiles_and_stubs_return_resp_error() {
        let mut b = Bridge::new();
        let reply = b.eval(b"return 1", &[], &[]);
        assert!(reply.starts_with(b"-ERR "));
        assert!(reply.ends_with(b"\r\n"));
        // SCRIPT EXISTS over an empty slice → empty Vec.
        assert!(b.script_exists(&[]).is_empty());
        // FlushMode round-trips.
        b.script_flush(FlushMode::Sync);
        b.script_flush(FlushMode::Async);
    }

    /// Sanity-check that luna-core's sandbox API path actually
    /// links — we don't yet build a Vm in production code but the
    /// dep wiring should already let one come up cleanly so P1 has
    /// nothing structural to fight.
    #[test]
    fn luna_core_dep_wires_through_sandbox_builder() {
        // P1 will replace the marker; for P0 the goal is purely
        // "this compiles and the linker resolves".
        let _v = LuaVersion::Lua51;
    }
}
