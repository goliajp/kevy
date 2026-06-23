//! Lua-side `redis` host table + KEYS / ARGV globals.
//!
//! Every per-dialect Vm gets one of these installed exactly once
//! (right after construction in `Bridge::vm_for`). The bindings are
//! per-Vm — every shard's bridge owns its own Vm pool, so the globals
//! never cross shard boundaries.
//!
//! P3a (this file): the redis.* surface is **stubbed**. `redis.call`
//! and `redis.pcall` raise a Lua error (so scripts that try to use
//! the keyspace fail loudly rather than silently misbehave); the
//! reply-builders (`status_reply`, `error_reply`) construct the right
//! tables; `sha1hex` / `log` / `replicate_commands` are no-op stubs.
//!
//! P3b will replace `redis.call` with the real HostDispatch
//! callback. The native fn signatures stay the same; only the body
//! grows.

use luna_core::runtime::value::Value;
use luna_core::vm::error::LuaError;
use luna_core::vm::exec::Vm;

/// Install the seven `redis.*` host methods and bind KEYS / ARGV
/// globals from this script's invocation. Called once per
/// `Bridge::eval` so subsequent EVALs see fresh argv tables.
///
/// `vm_install_redis_table` is called exactly once per Vm in
/// [`super::Bridge::vm_for`] — the table doesn't carry per-script
/// state, only KEYS/ARGV change between calls.
pub(crate) fn bind_keys_argv(vm: &mut Vm, keys: &[&[u8]], args: &[&[u8]]) {
    let keys_t = build_byte_array(vm, keys);
    let _ = vm.set_global("KEYS", Value::Table(keys_t));
    let argv_t = build_byte_array(vm, args);
    let _ = vm.set_global("ARGV", Value::Table(argv_t));
}

/// Install the `redis` host table once per Vm. Idempotent —
/// re-installing is a noop because Lua's `redis = {...}` just
/// rebinds the global.
pub(crate) fn install_redis_table(vm: &mut Vm) {
    // luna v1.1 B3: `vm.table_of` chains a fixed-size set of
    // (key, value) pairs into a freshly-allocated table. K=&str,
    // V=Value both impl IntoValue (luna v1.1 B4).
    // P3b: redis.call / redis.pcall now dispatch through the host
    // callback installed in the per-Vm userdata (see crate::dispatch).
    let call_fn = vm.native(crate::dispatch::redis_call);
    let pcall_fn = vm.native(crate::dispatch::redis_pcall);
    let status_fn = vm.native(redis_status_reply);
    let error_fn = vm.native(redis_error_reply);
    let sha1_fn = vm.native(redis_sha1hex);
    let log_fn = vm.native(redis_log);
    let replicate_fn = vm.native(redis_replicate_commands);
    let t = vm.table_of([
        ("call", call_fn),
        ("pcall", pcall_fn),
        ("status_reply", status_fn),
        ("error_reply", error_fn),
        ("sha1hex", sha1_fn),
        ("log", log_fn),
        ("replicate_commands", replicate_fn),
    ]);
    let _ = vm.set_global("redis", Value::Table(t));
}

fn build_byte_array(vm: &mut Vm, items: &[&[u8]]) -> luna_core::runtime::heap::Gc<luna_core::runtime::Table> {
    // Each item is an &[u8] (RESP bulk content, binary-safe). We
    // intern → Value::Str → set_int into a 1-indexed array.
    let mut entries: Vec<(i64, Value)> = Vec::with_capacity(items.len());
    for (i, bytes) in items.iter().enumerate() {
        let s = Value::Str(vm.heap.intern(bytes));
        entries.push(((i + 1) as i64, s));
    }
    // table_of needs a const-size array but we have a runtime length —
    // fall back to new_table + with(). Pre-sized via with_capacity on
    // the builder (luna v1.1 B3).
    let mut b = vm.new_table();
    for (k, v) in entries {
        b = b.with(k, v);
    }
    b.build()
}

// ─────────────────────────────────────────────────────────────────────
// redis.* host functions
// ─────────────────────────────────────────────────────────────────────

/// `redis.status_reply(msg)` — wraps `msg` into `{ok = msg}` which
/// marshals as a RESP simple string.
fn redis_status_reply(vm: &mut Vm, fs: u32, nargs: u32) -> Result<u32, LuaError> {
    let msg = first_arg_as_str_or(vm, fs, nargs, b"OK");
    let s = Value::Str(vm.heap.intern(&msg));
    let t = vm.new_table().with("ok", s).build();
    Ok(vm.nat_return(fs, &[Value::Table(t)]))
}

/// `redis.error_reply(msg)` — wraps `msg` into `{err = msg}` which
/// marshals as a RESP error.
fn redis_error_reply(vm: &mut Vm, fs: u32, nargs: u32) -> Result<u32, LuaError> {
    let msg = first_arg_as_str_or(vm, fs, nargs, b"ERR");
    let s = Value::Str(vm.heap.intern(&msg));
    let t = vm.new_table().with("err", s).build();
    Ok(vm.nat_return(fs, &[Value::Table(t)]))
}

/// `redis.sha1hex(s)` — SHA1 hex digest of the argument. Used by
/// scripts that pre-compute SCRIPT cache keys client-side.
fn redis_sha1hex(vm: &mut Vm, fs: u32, nargs: u32) -> Result<u32, LuaError> {
    let bytes = if nargs >= 1 {
        match vm.nat_arg(fs, nargs, 0) {
            Value::Str(s) => s.as_bytes().to_vec(),
            Value::Int(n) => n.to_string().into_bytes(),
            Value::Float(f) => format!("{f}").into_bytes(),
            _ => Vec::new(),
        }
    } else {
        Vec::new()
    };
    let digest = crate::sha1::sha1(&bytes);
    let hex = crate::sha1::hex(&digest);
    let v = Value::Str(vm.heap.intern(&hex));
    Ok(vm.nat_return(fs, &[v]))
}

/// `redis.log(level, msg)` — no-op stub. P5+ may wire to kevy's
/// slowlog or stderr. Returning no values matches Redis.
fn redis_log(_vm: &mut Vm, _fs: u32, _nargs: u32) -> Result<u32, LuaError> {
    Ok(0)
}

/// `redis.replicate_commands()` — Redis 7+ always-on noop. The
/// matching native fn keeps existing scripts that defensively call
/// it (BullMQ, Redlock variants) running unchanged.
fn redis_replicate_commands(_vm: &mut Vm, _fs: u32, _nargs: u32) -> Result<u32, LuaError> {
    Ok(0)
}

// ─────────────────────────────────────────────────────────────────────
// helpers
// ─────────────────────────────────────────────────────────────────────

/// Try to read the first arg as a Lua string. Falls back to
/// `default_msg` when the script passes `nil`, a non-string, or no
/// args at all. Same lenient shape PUC's `addReplyStatus` /
/// `addReplyErrorLength` use — scripts can call
/// `redis.error_reply()` with no args and get back `-ERR\r\n`.
fn first_arg_as_str_or(vm: &mut Vm, fs: u32, nargs: u32, default_msg: &[u8]) -> Vec<u8> {
    if nargs == 0 {
        return default_msg.to_vec();
    }
    match vm.nat_arg(fs, nargs, 0) {
        Value::Str(s) => s.as_bytes().to_vec(),
        Value::Int(n) => n.to_string().into_bytes(),
        Value::Float(f) => format!("{f}").into_bytes(),
        _ => default_msg.to_vec(),
    }
}

