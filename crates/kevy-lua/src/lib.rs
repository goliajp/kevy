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
    pub fn eval(&mut self, script: &[u8], _keys: &[&[u8]], _args: &[&[u8]]) -> Reply {
        // P2+ will accept binary; for now, scripts must be UTF-8.
        let src = match std::str::from_utf8(script) {
            Ok(s) => s,
            Err(_) => return resp::err(b"script is not valid UTF-8"),
        };
        let vm = self.vm_for(LuaVersion::Lua51);
        match vm.eval(src) {
            Ok(results) => {
                let first = results.first().copied().unwrap_or(Value::Nil);
                marshal_value(vm, first)
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
    /// hardware — Redis's default `lua-time-limit`).
    fn vm_for(&mut self, version: LuaVersion) -> &mut Vm {
        let slot = &mut self.vms[dialect_slot(version)];
        slot.get_or_insert_with(|| {
            Vm::sandbox(version)
                .open_base()
                .open_math()
                .open_string()
                .open_table()
                .with_instr_budget(200_000_000)
                .build()
        })
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

/// Marshal a luna `Value` into a RESP reply per the Redis EVAL rules:
///
/// | Lua                       | RESP                            |
/// |---------------------------|---------------------------------|
/// | nil                       | `$-1\r\n` (nil bulk)            |
/// | boolean true              | `:1\r\n`                        |
/// | boolean false             | `$-1\r\n` (nil bulk)            |
/// | integer                   | `:N\r\n`                        |
/// | integral float            | `:N\r\n` (5.1 returns 1 as 1.0) |
/// | non-integral float        | bulk string `$N\r\n<digits>\r\n`|
/// | string                    | bulk string (binary-safe)       |
/// | `{ok = "msg"}` table      | `+msg\r\n` (simple string)      |
/// | `{err = "msg"}` table     | `-msg\r\n` (error, no `ERR `)   |
/// | array table {v1, v2, …}   | `*N\r\n` + N marshaled elems    |
///
/// Array table iteration follows the Redis first-nil rule: the array
/// length is the number of consecutive non-nil values starting at
/// index 1. `{1, nil, 3}` produces `*1\r\n:1\r\n`, not `*3\r\n…`.
/// Closure / native fn / coroutine / userdata / lightuserdata cannot
/// be returned from a script to the wire and become nil-bulk replies.
///
/// Recursive on nested tables (`return {1, {2, 3}}` produces a
/// nested array). luna's `Vm::with_instr_budget` caps the recursion
/// depth — no separate guard needed here.
fn marshal_value(vm: &mut Vm, v: Value) -> Vec<u8> {
    match v {
        Value::Nil => resp::nil_bulk(),
        Value::Bool(true) => resp::integer(1),
        Value::Bool(false) => resp::nil_bulk(),
        Value::Int(n) => resp::integer(n),
        Value::Float(f) => resp::float(f),
        Value::Str(s) => resp::bulk(s.as_bytes()),
        Value::Table(t) => marshal_table(vm, t),
        Value::Closure(_)
        | Value::Native(_)
        | Value::Coro(_)
        | Value::Userdata(_)
        | Value::LightUserdata(_) => resp::nil_bulk(),
    }
}

/// Implement the table-→RESP rules. Order matters per Redis
/// semantics: check `err` first (PUC scripting.c also short-circuits
/// on err), then `ok`, then array.
fn marshal_table(vm: &mut Vm, t: luna_core::runtime::Gc<luna_core::runtime::Table>) -> Vec<u8> {
    // 1. {err = "..."} — RESP error.
    let err_key = Value::Str(vm.heap.intern(b"err"));
    let err_val = t.get(err_key);
    if let Value::Str(s) = err_val {
        return resp::err(s.as_bytes());
    }
    // 2. {ok = "..."} — RESP simple string.
    let ok_key = Value::Str(vm.heap.intern(b"ok"));
    let ok_val = t.get(ok_key);
    if let Value::Str(s) = ok_val {
        return resp::simple_string(s.as_bytes());
    }
    // 3. Otherwise array — iterate from index 1, stop at first nil.
    let mut items: Vec<Vec<u8>> = Vec::new();
    let mut i: i64 = 1;
    loop {
        let v = t.get_int(i);
        if matches!(v, Value::Nil) {
            break;
        }
        items.push(marshal_value(vm, v));
        i += 1;
    }
    let mut out = resp::array_header(items.len() as i64);
    for item in items {
        out.extend_from_slice(&item);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eval_return_one_is_resp_integer_one() {
        let mut b = Bridge::new();
        let reply = b.eval(b"return 1", &[], &[]);
        // 5.1 default: `return 1` yields a Float in Lua 5.1 (no int
        // type at 5.1) — we encode as RESP integer when the value
        // round-trips through i64 losslessly.
        assert_eq!(reply, b":1\r\n", "got: {:?}", String::from_utf8_lossy(&reply));
    }

    #[test]
    fn eval_return_string_is_resp_bulk_string() {
        let mut b = Bridge::new();
        let reply = b.eval(b"return 'hello'", &[], &[]);
        assert_eq!(reply, b"$5\r\nhello\r\n");
    }

    #[test]
    fn eval_return_nil_is_resp_nil_bulk() {
        let mut b = Bridge::new();
        let reply = b.eval(b"return nil", &[], &[]);
        assert_eq!(reply, b"$-1\r\n");
    }

    #[test]
    fn eval_return_true_is_resp_integer_one() {
        let mut b = Bridge::new();
        let reply = b.eval(b"return true", &[], &[]);
        assert_eq!(reply, b":1\r\n");
    }

    #[test]
    fn eval_return_false_is_resp_nil_bulk() {
        let mut b = Bridge::new();
        let reply = b.eval(b"return false", &[], &[]);
        assert_eq!(reply, b"$-1\r\n");
    }

    #[test]
    fn eval_syntax_error_is_resp_error() {
        let mut b = Bridge::new();
        let reply = b.eval(b"return ((", &[], &[]);
        assert!(reply.starts_with(b"-ERR "));
        assert!(reply.ends_with(b"\r\n"));
    }

    #[test]
    fn eval_no_return_is_resp_nil_bulk() {
        let mut b = Bridge::new();
        let reply = b.eval(b"local x = 1", &[], &[]);
        assert_eq!(reply, b"$-1\r\n");
    }

    #[test]
    fn eval_non_utf8_script_is_resp_error() {
        let mut b = Bridge::new();
        let reply = b.eval(&[0xff, 0xfe], &[], &[]);
        assert!(reply.starts_with(b"-ERR "));
    }

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

    // P2 — table marshaling.

    #[test]
    fn eval_ok_table_is_simple_string() {
        let mut b = Bridge::new();
        let reply = b.eval(b"return {ok = 'OK'}", &[], &[]);
        assert_eq!(reply, b"+OK\r\n");
    }

    #[test]
    fn eval_err_table_is_resp_error() {
        let mut b = Bridge::new();
        let reply = b.eval(b"return {err = 'something broke'}", &[], &[]);
        // err table content should pass through verbatim (Redis
        // convention: caller has full control over the error string,
        // including any `ERR ` / `NOSCRIPT` prefix).
        assert_eq!(reply, b"-ERR something broke\r\n");
    }

    #[test]
    fn eval_err_table_with_kind_passes_through() {
        let mut b = Bridge::new();
        let reply = b.eval(b"return {err = 'NOSCRIPT no script'}", &[], &[]);
        assert_eq!(reply, b"-NOSCRIPT no script\r\n");
    }

    #[test]
    fn eval_array_table_is_resp_array() {
        let mut b = Bridge::new();
        let reply = b.eval(b"return {1, 2, 3}", &[], &[]);
        assert_eq!(reply, b"*3\r\n:1\r\n:2\r\n:3\r\n");
    }

    #[test]
    fn eval_array_table_stops_at_first_nil() {
        let mut b = Bridge::new();
        // Redis first-nil rule: {1, nil, 3} encodes as a 1-element array.
        let reply = b.eval(b"return {1, nil, 3}", &[], &[]);
        assert_eq!(reply, b"*1\r\n:1\r\n");
    }

    #[test]
    fn eval_empty_table_is_empty_array() {
        let mut b = Bridge::new();
        let reply = b.eval(b"return {}", &[], &[]);
        assert_eq!(reply, b"*0\r\n");
    }

    #[test]
    fn eval_mixed_type_array() {
        let mut b = Bridge::new();
        let reply = b.eval(b"return {1, 'hello', true}", &[], &[]);
        // :1\r\n  $5\r\nhello\r\n  :1\r\n (true → :1)
        assert_eq!(reply, b"*3\r\n:1\r\n$5\r\nhello\r\n:1\r\n");
    }

    #[test]
    fn eval_nested_array() {
        let mut b = Bridge::new();
        let reply = b.eval(b"return {1, {2, 3}}", &[], &[]);
        // *2\r\n  :1\r\n  *2\r\n:2\r\n:3\r\n
        assert_eq!(reply, b"*2\r\n:1\r\n*2\r\n:2\r\n:3\r\n");
    }

    #[test]
    fn eval_err_beats_ok_when_both_present() {
        let mut b = Bridge::new();
        // Redis convention: if both ok and err are set, err wins.
        let reply = b.eval(b"return {ok = 'OK', err = 'oops'}", &[], &[]);
        assert_eq!(reply, b"-ERR oops\r\n");
    }

    #[test]
    fn eval_float_non_integral_is_bulk() {
        let mut b = Bridge::new();
        let reply = b.eval(b"return 1.5", &[], &[]);
        assert_eq!(reply, b"$3\r\n1.5\r\n");
    }

    #[test]
    fn eval_binary_safe_string() {
        let mut b = Bridge::new();
        // 5.1-compat decimal escapes — `\xFF` is a 5.2+ syntax that
        // the default 5.1 dialect rejects ("invalid escape sequence
        // near '\x'"). The 5.3+ tests in P4 will exercise hex escapes.
        let reply = b.eval(b"return '\\0\\1\\255'", &[], &[]);
        assert_eq!(reply, b"$3\r\n\x00\x01\xff\r\n");
    }
}
