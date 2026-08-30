//! `cmsgpack` Lua stdlib — Redis-compatible MessagePack encoder/decoder
//! implemented in pure Rust. Unblocks BullMQ scripts.
//!
//! ## Surface
//!
//! - `cmsgpack.pack(v1, v2, ...)` → string (concatenated msgpack bytes)
//! - `cmsgpack.unpack(str)` → multi-return values
//!
//! ## Type mapping (Lua → msgpack)
//!
//! | Lua | msgpack |
//! |---|---|
//! | nil | nil (`0xc0`) |
//! | bool | bool (`0xc2`/`0xc3`) |
//! | int | smallest int family (positive fixint / int8-64 / uint8-64) |
//! | float | float64 (`0xcb`) |
//! | string | str-family (UTF-8 not enforced — packs raw bytes) |
//! | array-shape table | array-family (`fixarray` / `array16` / `array32`) |
//! | mixed table | map-family (`fixmap` / `map16` / `map32`) |
//!
//! Array detection follows Redis cmsgpack: if `#table == N` and every
//! key 1..=N is present, encode as array. Otherwise, encode every
//! `(k, v)` from `Table::next` as a map.
//!
//! ## Implementation notes
//!
//! - Pure Rust, 0 third-party deps. Hand-written encoder + decoder
//!   per the [msgpack spec](https://github.com/msgpack/msgpack/blob/master/spec.md).
//! - No allocation beyond the output `Vec<u8>` and any tables the
//!   decoder constructs in the interpreter heap.
//! - Limits: max nesting depth 32 (mirrors the kevy-side recursion
//!   guard already in place for `redis.call` arrays). Beyond it,
//!   encoder errors out — matches Redis 7's behaviour.

use luna_core::runtime::value::Value;
use luna_core::vm::error::LuaError;
use luna_core::vm::exec::Vm;
// `Value` is itself IntoValue in luna-core, so we don't need to import
// the trait — TableBuilder::with accepts Values directly.

/// Max recursion depth — matches Redis 7's `cmsgpack` default.
const MAX_DEPTH: u32 = 32;

// ─────────────────────────────────────────────────────────────────────
// Public Lua bindings
// ─────────────────────────────────────────────────────────────────────

/// `cmsgpack.pack(arg1, arg2, ...)` → bulk string (concatenated
/// msgpack encoding of each argument in order).
pub(crate) fn cmsgpack_pack(vm: &mut Vm, fs: u32, nargs: u32) -> Result<u32, LuaError> {
    let mut out = Vec::with_capacity(64);
    for i in 0..nargs {
        let v = vm.nat_arg(fs, nargs, i);
        if let Err(e) = encode_value(vm, &v, &mut out, 0) {
            return Err(pack_error(vm, &e));
        }
    }
    let s = vm.heap.intern(&out);
    Ok(vm.nat_return(fs, &[Value::Str(s)]))
}

/// `cmsgpack.unpack(packed)` → values (multi-return). Decodes
/// successive msgpack values from the string until exhausted.
/// Trailing bytes (incomplete value) raise a Lua error.
pub(crate) fn cmsgpack_unpack(vm: &mut Vm, fs: u32, nargs: u32) -> Result<u32, LuaError> {
    let bytes = if nargs >= 1 {
        match vm.nat_arg(fs, nargs, 0) {
            Value::Str(s) => s.as_bytes().to_vec(),
            _ => return Err(unpack_error(vm, "cmsgpack.unpack expects a string argument")),
        }
    } else {
        return Err(unpack_error(vm, "cmsgpack.unpack expects a string argument"));
    };

    let mut cur = 0usize;
    let mut out: Vec<Value> = Vec::new();
    while cur < bytes.len() {
        match decode_value(vm, &bytes, &mut cur, 0) {
            Ok(v) => out.push(v),
            Err(e) => return Err(unpack_error(vm, &format!("cmsgpack.unpack: {e}"))),
        }
    }
    Ok(vm.nat_return(fs, &out))
}

// ─────────────────────────────────────────────────────────────────────
// Encoder
// ─────────────────────────────────────────────────────────────────────

#[path = "cmsgpack_encode.rs"]
mod encode;
use encode::encode_value;

// fn-length exemption: pure data-driven msgpack tag match table — one
// flat arm per wire tag, only a length read + decode_* delegate each.
// LOC-WAIVER: data-driven msgpack tag dispatch table — one arm per tag range.
fn decode_value(vm: &mut Vm, bytes: &[u8], cur: &mut usize, depth: u32) -> Result<Value, String> {
    if depth >= MAX_DEPTH {
        return Err("max recursion depth".into());
    }
    if *cur >= bytes.len() {
        return Err("unexpected end of input".into());
    }
    let tag = bytes[*cur];
    *cur += 1;
    match tag {
        // positive fixint
        0x00..=0x7f => Ok(Value::Int(tag as i64)),
        // fixmap
        0x80..=0x8f => {
            let n = (tag & 0x0f) as usize;
            decode_map(vm, bytes, cur, n, depth + 1)
        }
        // fixarray
        0x90..=0x9f => {
            let n = (tag & 0x0f) as usize;
            decode_array(vm, bytes, cur, n, depth + 1)
        }
        // fixstr
        0xa0..=0xbf => {
            let n = (tag & 0x1f) as usize;
            decode_str(vm, bytes, cur, n)
        }
        0xc0 => Ok(Value::Nil),
        0xc1 => Err("reserved msgpack tag 0xc1".into()),
        0xc2 => Ok(Value::Bool(false)),
        0xc3 => Ok(Value::Bool(true)),
        // bin8/16/32 — decode as Lua string (Redis cmsgpack semantics)
        0xc4 => {
            let n = read_u8(bytes, cur)? as usize;
            decode_str(vm, bytes, cur, n)
        }
        0xc5 => {
            let n = read_u16(bytes, cur)? as usize;
            decode_str(vm, bytes, cur, n)
        }
        0xc6 => {
            let n = read_u32(bytes, cur)? as usize;
            decode_str(vm, bytes, cur, n)
        }
        // float32 → Float
        0xca => {
            if *cur + 4 > bytes.len() {
                return Err("short read float32".into());
            }
            let f = f32::from_be_bytes([
                bytes[*cur],
                bytes[*cur + 1],
                bytes[*cur + 2],
                bytes[*cur + 3],
            ]);
            *cur += 4;
            Ok(Value::Float(f as f64))
        }
        0xcb => {
            if *cur + 8 > bytes.len() {
                return Err("short read float64".into());
            }
            let mut a = [0u8; 8];
            a.copy_from_slice(&bytes[*cur..*cur + 8]);
            *cur += 8;
            Ok(Value::Float(f64::from_be_bytes(a)))
        }
        0xcc => Ok(Value::Int(read_u8(bytes, cur)? as i64)),
        0xcd => Ok(Value::Int(read_u16(bytes, cur)? as i64)),
        0xce => Ok(Value::Int(read_u32(bytes, cur)? as i64)),
        0xcf => {
            let n = read_u64(bytes, cur)?;
            // u64 → i64; values > i64::MAX become negative on cast,
            // matching Redis (Lua 5.1 has no unsigned).
            Ok(Value::Int(n as i64))
        }
        0xd0 => Ok(Value::Int(read_u8(bytes, cur)? as i8 as i64)),
        0xd1 => Ok(Value::Int(read_u16(bytes, cur)? as i16 as i64)),
        0xd2 => Ok(Value::Int(read_u32(bytes, cur)? as i32 as i64)),
        0xd3 => Ok(Value::Int(read_u64(bytes, cur)? as i64)),
        // str8/16/32
        0xd9 => {
            let n = read_u8(bytes, cur)? as usize;
            decode_str(vm, bytes, cur, n)
        }
        0xda => {
            let n = read_u16(bytes, cur)? as usize;
            decode_str(vm, bytes, cur, n)
        }
        0xdb => {
            let n = read_u32(bytes, cur)? as usize;
            decode_str(vm, bytes, cur, n)
        }
        // array16/32
        0xdc => {
            let n = read_u16(bytes, cur)? as usize;
            decode_array(vm, bytes, cur, n, depth + 1)
        }
        0xdd => {
            let n = read_u32(bytes, cur)? as usize;
            decode_array(vm, bytes, cur, n, depth + 1)
        }
        // map16/32
        0xde => {
            let n = read_u16(bytes, cur)? as usize;
            decode_map(vm, bytes, cur, n, depth + 1)
        }
        0xdf => {
            let n = read_u32(bytes, cur)? as usize;
            decode_map(vm, bytes, cur, n, depth + 1)
        }
        // negative fixint
        0xe0..=0xff => Ok(Value::Int(tag as i8 as i64)),
        // ext types — not commonly used by BullMQ; return as nil for
        // forward-compat or error. We error to surface unknown data.
        _ => Err(format!("unsupported msgpack tag 0x{tag:02x}")),
    }
}

fn decode_str(vm: &mut Vm, bytes: &[u8], cur: &mut usize, n: usize) -> Result<Value, String> {
    if *cur + n > bytes.len() {
        return Err("short read str".into());
    }
    let s = vm.heap.intern(&bytes[*cur..*cur + n]);
    *cur += n;
    Ok(Value::Str(s))
}

/// Upper bound on the initial reservation for an element count read out of
/// a msgpack header. The smallest value is one byte (a fixint), so
/// `remaining` bytes cannot supply more than `remaining` elements, and a map
/// entry is a key and a value. Not a limit on the decode, which errors on
/// the first element the input cannot supply. The measurement, and why this
/// is reachable from any client, is in `cmsgpack_tests.rs`.
fn elements_fit(n: usize, remaining: usize, per_element: usize) -> usize {
    n.min(remaining / per_element + 1)
}

fn decode_array(
    vm: &mut Vm,
    bytes: &[u8],
    cur: &mut usize,
    n: usize,
    depth: u32,
) -> Result<Value, String> {
    let mut entries: Vec<Value> =
        Vec::with_capacity(elements_fit(n, bytes.len().saturating_sub(*cur), 1));
    for _ in 0..n {
        entries.push(decode_value(vm, bytes, cur, depth)?);
    }
    let mut b = vm.new_table();
    for (i, v) in entries.into_iter().enumerate() {
        b = b.with((i + 1) as i64, v);
    }
    Ok(Value::Table(b.build()))
}

fn decode_map(
    vm: &mut Vm,
    bytes: &[u8],
    cur: &mut usize,
    n: usize,
    depth: u32,
) -> Result<Value, String> {
    // Pre-collect k/v so we have no &mut Vm conflict with the builder.
    let mut kvs: Vec<(Value, Value)> =
        Vec::with_capacity(elements_fit(n, bytes.len().saturating_sub(*cur), 2));
    for _ in 0..n {
        let k = decode_value(vm, bytes, cur, depth)?;
        let v = decode_value(vm, bytes, cur, depth)?;
        kvs.push((k, v));
    }
    let mut b = vm.new_table();
    for (k, v) in kvs {
        b = b.with(k, v);
    }
    Ok(Value::Table(b.build()))
}

fn read_u8(bytes: &[u8], cur: &mut usize) -> Result<u8, String> {
    if *cur >= bytes.len() {
        return Err("short read u8".into());
    }
    let n = bytes[*cur];
    *cur += 1;
    Ok(n)
}

fn read_u16(bytes: &[u8], cur: &mut usize) -> Result<u16, String> {
    if *cur + 2 > bytes.len() {
        return Err("short read u16".into());
    }
    let n = u16::from_be_bytes([bytes[*cur], bytes[*cur + 1]]);
    *cur += 2;
    Ok(n)
}

fn read_u32(bytes: &[u8], cur: &mut usize) -> Result<u32, String> {
    if *cur + 4 > bytes.len() {
        return Err("short read u32".into());
    }
    let n = u32::from_be_bytes([bytes[*cur], bytes[*cur + 1], bytes[*cur + 2], bytes[*cur + 3]]);
    *cur += 4;
    Ok(n)
}

fn read_u64(bytes: &[u8], cur: &mut usize) -> Result<u64, String> {
    if *cur + 8 > bytes.len() {
        return Err("short read u64".into());
    }
    let mut a = [0u8; 8];
    a.copy_from_slice(&bytes[*cur..*cur + 8]);
    *cur += 8;
    Ok(u64::from_be_bytes(a))
}

fn pack_error(vm: &mut Vm, msg: &str) -> LuaError {
    let s = vm.heap.intern(format!("cmsgpack.pack: {msg}").as_bytes());
    LuaError::new(Value::Str(s))
}

fn unpack_error(vm: &mut Vm, msg: &str) -> LuaError {
    let s = vm.heap.intern(msg.as_bytes());
    LuaError::new(Value::Str(s))
}

// ─────────────────────────────────────────────────────────────────────
// Installation
// ─────────────────────────────────────────────────────────────────────

pub(crate) fn install_cmsgpack(vm: &mut Vm) {
    let pack_fn = vm.native(cmsgpack_pack);
    let unpack_fn = vm.native(cmsgpack_unpack);
    let t = vm.table_of([("pack", pack_fn), ("unpack", unpack_fn)]);
    let _ = vm.set_global("cmsgpack", Value::Table(t));
}

#[cfg(test)]
#[path = "cmsgpack_tests.rs"]
mod bound_tests;
