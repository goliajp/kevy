//! Host-dispatch plumbing — the real `redis.call` / `redis.pcall`.
//!
//! P3b wires the `redis.call` native fn to a host-side closure that
//! returns RESP reply bytes. In the kevy-rt integration that closure
//! reaches the normal command dispatch path; in tests it's a stub
//! in-memory keyspace.
//!
//! ## Flow
//!
//! 1. `Bridge::new(dispatch)` stores `Rc<dyn Fn(&[&[u8]]) -> Vec<u8>>`.
//! 2. `Bridge::vm_for` installs an `Rc::clone` as a luna userdata global
//!    `"__kevy_dispatch"` once per Vm (luna v1.1 B8).
//! 3. `redis_call` / `redis_pcall` native fns:
//!    - read the dispatch handle via `vm.userdata_borrow`,
//!    - collect Lua arg values into `Vec<Vec<u8>>` (binary-safe via
//!      `Value::Str(..).as_bytes()`),
//!    - call the host fn with `&[&[u8]]`,
//!    - parse the RESP reply bytes back into a `Value` and return it.
//!
//! ## RESP → Lua marshaling
//!
//! Symmetric inverse of `crate::marshal`:
//!
//! | RESP                      | Lua (`call`)                | Lua (`pcall`)            |
//! |---------------------------|-----------------------------|--------------------------|
//! | `+OK\r\n`                 | `{ok = "OK"}` table         | same                     |
//! | `-ERR msg\r\n`            | raise Lua error (`call`)    | `{err = "msg"}` table    |
//! | `:N\r\n`                  | integer                     | same                     |
//! | `$N\r\n…\r\n`             | string                      | same                     |
//! | `$-1\r\n`                 | boolean `false`             | same                     |
//! | `*N\r\n…`                 | 1-indexed table             | same                     |

use luna_core::runtime::value::Value;
use luna_core::vm::error::LuaError;
use luna_core::vm::exec::Vm;
use std::cell::Cell;
use std::rc::Rc;

/// Dispatch callable shape. The `read_only` flag is set by
/// [`crate::Bridge::eval_ro`] / [`crate::Bridge::evalsha_ro`]; the
/// dispatcher is responsible for rejecting write commands with
/// `-READONLY can't write against a read-only script\r\n` (kevy-rt
/// owns the canonical command-flag table and does this check
/// natively). Tests provide a minimal in-memory dispatcher that
/// hard-codes a few write commands.
pub type DispatchFn = dyn Fn(&[&[u8]], bool) -> Vec<u8>;

/// The boxed handle Bridge owns + every per-dialect Vm holds via
/// userdata. `Rc` so it's `Clone` for the install-per-Vm path and
/// `Any + 'static` so luna's userdata can store it.
pub type DispatchHandle = Rc<DispatchFn>;

/// Wrapper that luna's userdata can downcast back to. Carries:
/// - `f`: the user-supplied dispatch callback;
/// - `read_only`: shared mode flag set by `Bridge::eval_ro` /
///   `evalsha_ro` before invoking `vm.eval` and cleared right after.
///   Native fns read this when dispatching so the flag travels into
///   the user's closure without changing the closure type.
///
/// `Rc<Cell<bool>>` so every per-dialect Vm sees the same mode bit
/// without us having to walk the Vm pool when toggling.
pub(crate) struct DispatchSlot {
    pub f: DispatchHandle,
    pub read_only: Rc<Cell<bool>>,
}

/// Global name we stash the dispatch userdata under. Scripts never
/// reference it directly; the leading underscores match the
/// "host-private global" convention.
pub(crate) const DISPATCH_KEY: &str = "__kevy_dispatch";

// ─────────────────────────────────────────────────────────────────────
// `redis.call` / `redis.pcall` — real implementations
// ─────────────────────────────────────────────────────────────────────

/// The `call` form: a `-ERR …` RESP reply (any reply starting with
/// `-`) raises a Lua error so the script can `pcall` to catch.
pub(crate) fn redis_call(vm: &mut Vm, fs: u32, nargs: u32) -> Result<u32, LuaError> {
    let reply = invoke_dispatch(vm, fs, nargs)?;
    if reply.starts_with(b"-") {
        // RESP error → raise Lua error with the message (skip the
        // leading `-` and the trailing `\r\n`). Match Redis behaviour:
        // the Lua-side error message reads the same as the wire form
        // minus framing, including any kind prefix like `NOSCRIPT `.
        let msg = error_payload(&reply);
        let v = Value::Str(vm.heap.intern(&msg));
        return Err(LuaError(v));
    }
    let v = parse_resp_value(vm, &reply, &mut 0)?;
    Ok(vm.nat_return(fs, &[v]))
}

/// The `pcall` form: a `-ERR …` reply becomes a `{err = "msg"}`
/// table that the marshaler can then surface as a RESP error. Lua
/// errors raised from the dispatch fn itself also surface here (not
/// reachable in the in-memory stub, but the contract is set for
/// the kevy-rt integration).
pub(crate) fn redis_pcall(vm: &mut Vm, fs: u32, nargs: u32) -> Result<u32, LuaError> {
    let reply = invoke_dispatch(vm, fs, nargs)?;
    if reply.starts_with(b"-") {
        let msg = error_payload(&reply);
        let s = Value::Str(vm.heap.intern(&msg));
        let t = vm.new_table().with("err", s).build();
        return Ok(vm.nat_return(fs, &[Value::Table(t)]));
    }
    let v = parse_resp_value(vm, &reply, &mut 0)?;
    Ok(vm.nat_return(fs, &[v]))
}

// ─────────────────────────────────────────────────────────────────────
// helpers
// ─────────────────────────────────────────────────────────────────────

/// Read the dispatch userdata, collect args, dispatch, return reply.
fn invoke_dispatch(vm: &mut Vm, fs: u32, nargs: u32) -> Result<Vec<u8>, LuaError> {
    let (dispatch, read_only) = vm
        .userdata_borrow::<DispatchSlot>(DISPATCH_KEY)
        .map(|s| (Rc::clone(&s.f), Rc::clone(&s.read_only)))
        .ok_or_else(|| {
            LuaError(Value::Str(
                vm.heap
                    .intern(b"redis.call: kevy host dispatch not installed"),
            ))
        })?;
    if nargs == 0 {
        return Err(LuaError(Value::Str(
            vm.heap
                .intern(b"redis.call: wrong number of arguments (no command)"),
        )));
    }
    let mut argv: Vec<Vec<u8>> = Vec::with_capacity(nargs as usize);
    for i in 0..nargs {
        let v = vm.nat_arg(fs, nargs, i);
        argv.push(value_to_bytes(v));
    }
    let arg_refs: Vec<&[u8]> = argv.iter().map(|v| v.as_slice()).collect();
    Ok(dispatch(&arg_refs, read_only.get()))
}

/// Lua Value → wire bytes for redis.call args. Strings pass through
/// binary-safe; numbers stringify decimal; nil / bool / table / fn
/// produce empty bytes (Redis semantics: redis.call args must be
/// numbers or strings).
fn value_to_bytes(v: Value) -> Vec<u8> {
    match v {
        Value::Str(s) => s.as_bytes().to_vec(),
        Value::Int(n) => n.to_string().into_bytes(),
        Value::Float(f) => {
            if f.is_finite() && f.fract() == 0.0
                && (i64::MIN as f64..=i64::MAX as f64).contains(&f)
            {
                (f as i64).to_string().into_bytes()
            } else {
                format!("{f}").into_bytes()
            }
        }
        _ => Vec::new(),
    }
}

/// Extract the RESP error payload from `-MSG\r\n` (strips `-` and
/// `\r\n`). Used by both call (raise) and pcall (`{err = msg}`).
fn error_payload(reply: &[u8]) -> Vec<u8> {
    let mut s = reply;
    if s.starts_with(b"-") {
        s = &s[1..];
    }
    if s.ends_with(b"\r\n") {
        s = &s[..s.len() - 2];
    }
    s.to_vec()
}

/// Parse a single RESP value starting at `bytes[*pos]`, advancing
/// `*pos`. Recursive on arrays. Returns Lua-side Value per the
/// marshaling table.
fn parse_resp_value(vm: &mut Vm, bytes: &[u8], pos: &mut usize) -> Result<Value, LuaError> {
    if *pos >= bytes.len() {
        return Err(LuaError(Value::Str(
            vm.heap.intern(b"redis.call: empty RESP reply"),
        )));
    }
    let tag = bytes[*pos];
    *pos += 1;
    match tag {
        b'+' => {
            // Simple string `+OK\r\n` → {ok="OK"} table.
            let line = read_line(bytes, pos)?;
            let s = Value::Str(vm.heap.intern(&line));
            let t = vm.new_table().with("ok", s).build();
            Ok(Value::Table(t))
        }
        b':' => {
            // Integer.
            let line = read_line(bytes, pos)?;
            let n = std::str::from_utf8(&line)
                .map_err(|_| {
                    LuaError(Value::Str(
                        vm.heap.intern(b"redis.call: invalid integer reply"),
                    ))
                })?
                .parse::<i64>()
                .map_err(|_| {
                    LuaError(Value::Str(
                        vm.heap.intern(b"redis.call: invalid integer reply"),
                    ))
                })?;
            Ok(Value::Int(n))
        }
        b'$' => {
            // Bulk string `$N\r\nbytes\r\n` or nil bulk `$-1\r\n`.
            let len_line = read_line(bytes, pos)?;
            let n: i64 = std::str::from_utf8(&len_line)
                .map_err(|_| {
                    LuaError(Value::Str(vm.heap.intern(b"redis.call: invalid bulk length")))
                })?
                .parse()
                .map_err(|_| {
                    LuaError(Value::Str(vm.heap.intern(b"redis.call: invalid bulk length")))
                })?;
            if n < 0 {
                return Ok(Value::Bool(false));
            }
            let len = n as usize;
            if *pos + len + 2 > bytes.len() {
                return Err(LuaError(Value::Str(
                    vm.heap.intern(b"redis.call: truncated bulk reply"),
                )));
            }
            let payload = &bytes[*pos..*pos + len];
            let v = Value::Str(vm.heap.intern(payload));
            *pos += len + 2; // skip payload + \r\n
            Ok(v)
        }
        b'*' => {
            // Array.
            let len_line = read_line(bytes, pos)?;
            let n: i64 = std::str::from_utf8(&len_line)
                .map_err(|_| {
                    LuaError(Value::Str(vm.heap.intern(b"redis.call: invalid array length")))
                })?
                .parse()
                .map_err(|_| {
                    LuaError(Value::Str(vm.heap.intern(b"redis.call: invalid array length")))
                })?;
            if n < 0 {
                return Ok(Value::Bool(false));
            }
            let count = n as usize;
            let mut entries: Vec<(i64, Value)> = Vec::with_capacity(count);
            for i in 0..count {
                let v = parse_resp_value(vm, bytes, pos)?;
                entries.push(((i + 1) as i64, v));
            }
            let mut b = vm.new_table();
            for (k, v) in entries {
                b = b.with(k, v);
            }
            Ok(Value::Table(b.build()))
        }
        _ => Err(LuaError(Value::Str(
            vm.heap.intern(b"redis.call: unknown RESP type tag"),
        ))),
    }
}

/// Read until `\r\n`, advancing `pos` past the CRLF. Returns the
/// bytes before the CRLF.
fn read_line(bytes: &[u8], pos: &mut usize) -> Result<Vec<u8>, LuaError> {
    let start = *pos;
    while *pos + 1 < bytes.len() {
        if bytes[*pos] == b'\r' && bytes[*pos + 1] == b'\n' {
            let line = bytes[start..*pos].to_vec();
            *pos += 2;
            return Ok(line);
        }
        *pos += 1;
    }
    Err(LuaError(Value::Nil))
}
