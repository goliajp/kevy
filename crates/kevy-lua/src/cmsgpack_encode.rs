//! The encode half of `cmsgpack` — split from the decode half at the
//! 500-line house rule. Nothing here reads untrusted lengths; it walks a
//! Lua table the script already built.

use luna_core::runtime::heap::Gc;
use luna_core::runtime::table::Table;
use luna_core::runtime::value::Value;
use luna_core::vm::exec::Vm;

use super::MAX_DEPTH;

pub(super) fn encode_value(
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
    } else if (0..=0xff).contains(&n) {
        out.push(0xcc);
        out.push(n as u8);
    } else if (-0x8000..=0x7fff).contains(&n) {
        out.push(0xd1);
        out.extend_from_slice(&(n as i16).to_be_bytes());
    } else if (0..=0xffff).contains(&n) {
        out.push(0xcd);
        out.extend_from_slice(&(n as u16).to_be_bytes());
    } else if (-0x8000_0000..=0x7fff_ffff).contains(&n) {
        out.push(0xd2);
        out.extend_from_slice(&(n as i32).to_be_bytes());
    } else if (0..=0xffff_ffff).contains(&n) {
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

fn encode_table(vm: &Vm, t: Gc<Table>, out: &mut Vec<u8>, depth: u32) -> Result<(), String> {
    let t_ref = &*t;
    let (n, total_entries, is_array) = table_shape(t_ref)?;
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

/// Array-shape detection: `len()` gives N if 1..=N are all non-nil.
/// Confirm every key 1..=len is present (`len()` returns N even if
/// there are extra non-integer keys). We treat a table as an array
/// iff (a) len > 0 and (b) iterating from key 1..=N yields all
/// non-nil values AND (c) total entry count == N (no extra keys).
/// Returns `(len, total_entries, is_array)`.
fn table_shape(t_ref: &Table) -> Result<(usize, usize, bool), String> {
    let len = t_ref.len();
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
    Ok((n, total_entries, is_array))
}

// ─────────────────────────────────────────────────────────────────────
// Decoder
// ─────────────────────────────────────────────────────────────────────
