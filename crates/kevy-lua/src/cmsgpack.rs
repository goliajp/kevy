//! `cmsgpack` Lua stdlib — Redis-compatible MessagePack encoder/decoder
//! implemented in pure Rust. v1.27.3 BullMQ unblock.
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
//!   decoder constructs in the luna heap.
//! - Limits: max nesting depth 32 (mirrors the kevy-side recursion
//!   guard already in place for `redis.call` arrays). Beyond it,
//!   encoder errors out — matches Redis 7's behaviour.

use luna_core::runtime::heap::Gc;
use luna_core::runtime::table::Table;
use luna_core::runtime::value::Value;
use luna_core::vm::error::LuaError;
use luna_core::vm::exec::Vm;
// `Value` is itself IntoValue (luna v1.1) so we don't need to import
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

fn encode_value(
    vm: &Vm,
    v: &Value,
    out: &mut Vec<u8>,
    depth: u32,
) -> Result<(), String> {
    if depth >= MAX_DEPTH {
        return Err("cmsgpack: max recursion depth exceeded".into());
    }
    match v {
        Value::Nil => out.push(0xc0),
        Value::Bool(b) => out.push(if *b { 0xc3 } else { 0xc2 }),
        Value::Int(n) => encode_int(*n, out),
        Value::Float(f) => encode_float(*f, out),
        Value::Str(s) => encode_str(s.as_bytes(), out),
        Value::Table(t) => encode_table(vm, *t, out, depth + 1)?,
        _ => return Err("cmsgpack: unsupported Lua type".into()),
    }
    Ok(())
}

fn encode_int(n: i64, out: &mut Vec<u8>) {
    if (0..=0x7f).contains(&n) {
        // positive fixint
        out.push(n as u8);
    } else if (-32..0).contains(&n) {
        // negative fixint
        out.push((n as i8) as u8);
    } else if (-0x80..=0x7f).contains(&n) {
        out.push(0xd0);
        out.push(n as u8);
    } else if n >= 0 && n <= 0xff {
        out.push(0xcc);
        out.push(n as u8);
    } else if (-0x8000..=0x7fff).contains(&n) {
        out.push(0xd1);
        out.extend_from_slice(&(n as i16).to_be_bytes());
    } else if n >= 0 && n <= 0xffff {
        out.push(0xcd);
        out.extend_from_slice(&(n as u16).to_be_bytes());
    } else if (-0x8000_0000..=0x7fff_ffff).contains(&n) {
        out.push(0xd2);
        out.extend_from_slice(&(n as i32).to_be_bytes());
    } else if n >= 0 && n <= 0xffff_ffff {
        out.push(0xce);
        out.extend_from_slice(&(n as u32).to_be_bytes());
    } else if n >= 0 {
        out.push(0xcf);
        out.extend_from_slice(&(n as u64).to_be_bytes());
    } else {
        out.push(0xd3);
        out.extend_from_slice(&n.to_be_bytes());
    }
}

fn encode_float(f: f64, out: &mut Vec<u8>) {
    // Redis cmsgpack: collapse integral floats to int family so that
    // Lua 5.1's number type (always float) round-trips byte-identical
    // through the integer path. Same rule kevy's RESP marshaling
    // applies for the same reason.
    if f.is_finite() && f.fract() == 0.0 && (i64::MIN as f64..=i64::MAX as f64).contains(&f) {
        encode_int(f as i64, out);
        return;
    }
    out.push(0xcb);
    out.extend_from_slice(&f.to_be_bytes());
}

fn encode_str(bytes: &[u8], out: &mut Vec<u8>) {
    let len = bytes.len();
    if len <= 31 {
        out.push(0xa0 | (len as u8));
    } else if len <= 0xff {
        out.push(0xd9);
        out.push(len as u8);
    } else if len <= 0xffff {
        out.push(0xda);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(0xdb);
        out.extend_from_slice(&(len as u32).to_be_bytes());
    }
    out.extend_from_slice(bytes);
}

fn encode_table(
    vm: &Vm,
    t: Gc<Table>,
    out: &mut Vec<u8>,
    depth: u32,
) -> Result<(), String> {
    let t_ref = &*t;
    let len = t_ref.len();
    // Array-shape detection: len() gives N if 1..=N are all non-nil.
    // Confirm every key 1..=len is present (len() returns N even if
    // there are extra non-integer keys). We treat a table as an array
    // iff (a) len > 0 and (b) iterating from key 1..=N yields all
    // non-nil values AND (c) total entry count == N (no extra keys).
    let mut total_entries = 0usize;
    let mut k = Value::Nil;
    while let Some((nk, _)) = t_ref.next(k).map_err(|e| format!("table iter: {e:?}"))? {
        total_entries += 1;
        k = nk;
    }
    let n = len as usize;
    let mut is_array = false;
    if n > 0 && n == total_entries {
        // Verify keys 1..=n are present.
        let mut ok = true;
        for i in 1..=n {
            if matches!(t_ref.get(Value::Int(i as i64)), Value::Nil) {
                ok = false;
                break;
            }
        }
        is_array = ok;
    }
    if is_array {
        // Array header
        if n <= 15 {
            out.push(0x90 | (n as u8));
        } else if n <= 0xffff {
            out.push(0xdc);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        } else {
            out.push(0xdd);
            out.extend_from_slice(&(n as u32).to_be_bytes());
        }
        for i in 1..=n {
            let v = t_ref.get(Value::Int(i as i64));
            encode_value(vm, &v, out, depth)?;
        }
    } else {
        // Map header
        let m = total_entries;
        if m <= 15 {
            out.push(0x80 | (m as u8));
        } else if m <= 0xffff {
            out.push(0xde);
            out.extend_from_slice(&(m as u16).to_be_bytes());
        } else {
            out.push(0xdf);
            out.extend_from_slice(&(m as u32).to_be_bytes());
        }
        let mut k = Value::Nil;
        while let Some((nk, v)) = t_ref.next(k).map_err(|e| format!("table iter: {e:?}"))? {
            encode_value(vm, &nk, out, depth)?;
            encode_value(vm, &v, out, depth)?;
            k = nk;
        }
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────
// Decoder
// ─────────────────────────────────────────────────────────────────────

fn decode_value(
    vm: &mut Vm,
    bytes: &[u8],
    cur: &mut usize,
    depth: u32,
) -> Result<Value, String> {
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

fn decode_array(
    vm: &mut Vm,
    bytes: &[u8],
    cur: &mut usize,
    n: usize,
    depth: u32,
) -> Result<Value, String> {
    let mut entries: Vec<Value> = Vec::with_capacity(n);
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
    let mut kvs: Vec<(Value, Value)> = Vec::with_capacity(n);
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

