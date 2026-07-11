//! Chunk readers, row emission and wire-shape helpers shared by the
//! IDX.* reduces (and, via the `*_at` shims, the VIEW.* reduce).

use kevy_index::IndexValue;
use kevy_resp::{encode_array_len, encode_bulk};

use crate::cmd_index_query::{Hydrated, encode_value, hex};

pub(super) fn read_u32(c: &[u8], pos: &mut usize) -> Option<u32> {
    let v = u32::from_le_bytes(c.get(*pos..*pos + 4)?.try_into().ok()?);
    *pos += 4;
    Some(v)
}

pub(super) fn read_kbytes(c: &[u8], pos: &mut usize) -> Option<Vec<u8>> {
    let n = read_u32(c, pos)? as usize;
    let b = c.get(*pos..*pos + n)?.to_vec();
    *pos += n;
    Some(b)
}

pub(super) fn read_hydration(c: &[u8], pos: &mut usize) -> Option<Hydrated> {
    let n = *c.get(*pos)? as usize;
    *pos += 1;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let len = read_u32(c, pos)?;
        if len == u32::MAX {
            out.push(None);
        } else {
            let b = c.get(*pos..*pos + len as usize)?.to_vec();
            *pos += len as usize;
            out.push(Some(b));
        }
    }
    Some(out)
}

/// One hydrated row: `*(1|2)+2F [key, value?, (fname, fval|nil)…]`.
pub(super) fn emit_row(
    out: &mut Vec<u8>,
    key: &[u8],
    value: Option<&IndexValue>,
    fv: &Hydrated,
    fields: &[Vec<u8>],
) {
    let base = 1 + usize::from(value.is_some());
    encode_array_len(out, (base + fields.len() * 2) as i64);
    encode_bulk(out, key);
    if let Some(v) = value {
        encode_bulk(out, &value_repr(v));
    }
    for (f, v) in fields.iter().zip(fv) {
        encode_bulk(out, f);
        match v {
            Some(b) => encode_bulk(out, b),
            None => out.extend_from_slice(b"$-1\r\n"),
        }
    }
}

pub(super) fn value_repr(v: &IndexValue) -> Vec<u8> {
    match v {
        IndexValue::I64(i) => i.to_string().into_bytes(),
        IndexValue::F64(f) => format!("{f}").into_bytes(),
        IndexValue::Str(s) => s.clone(),
    }
}

/// The view reduce reuses the (value,key) cursor encoding.
pub(crate) fn encode_view_cursor_bytes(v: &IndexValue, k: &[u8]) -> Vec<u8> {
    encode_cursor(v, k)
}

/// Shared chunk readers + value repr for the view reduce.
pub(crate) fn read_u32_at(c: &[u8], pos: &mut usize) -> Option<u32> {
    read_u32(c, pos)
}

/// See [`read_u32_at`].
pub(crate) fn read_kbytes_at(c: &[u8], pos: &mut usize) -> Option<Vec<u8>> {
    read_kbytes(c, pos)
}

/// See [`read_u32_at`].
pub(crate) fn value_repr_pub(v: &IndexValue) -> Vec<u8> {
    value_repr(v)
}

pub(super) fn encode_cursor(v: &IndexValue, k: &[u8]) -> Vec<u8> {
    let mut payload = Vec::new();
    encode_value(&mut payload, v);
    payload.extend_from_slice(k);
    hex(&payload)
}

/// RESP3 upgrade for extension replies: pair-array shaped
/// verbs (IDX.EXPLAIN) re-emit as a Map on a HELLO 3 conn. Purely a
/// wire-shape transform of the already-reduced RESP2 bytes: `*N` of
/// 2-arrays → `%N/2` of flat pairs. Verbs whose replies are NOT
/// key/value pairs pass through untouched (spec-legal gradual
/// migration, same posture as `dispatch_resp3.rs`'s overrides).
pub(crate) fn resp3_upgrade(argv: &[Vec<u8>], reply: Vec<u8>) -> Vec<u8> {
    let verb = argv.first().map(Vec::as_slice).unwrap_or(b"");
    let mapify = verb.eq_ignore_ascii_case(b"IDX.EXPLAIN")
        || verb.eq_ignore_ascii_case(b"VIEW.EXPLAIN");
    if !mapify || !reply.starts_with(b"*") {
        return reply;
    }
    // Parse `*N\r\n` then N × (`*2\r\n` pair); bail untouched on any
    // shape surprise.
    let Some(hdr_end) = reply.iter().position(|&b| b == b'\n') else { return reply };
    let Ok(n) = std::str::from_utf8(&reply[1..hdr_end - 1])
        .unwrap_or("x")
        .parse::<usize>()
    else {
        return reply;
    };
    let body = &reply[hdr_end + 1..];
    let mut out = Vec::with_capacity(reply.len());
    out.extend_from_slice(format!("%{n}\r\n").as_bytes());
    let mut rest = body;
    for _ in 0..n {
        if !rest.starts_with(b"*2\r\n") {
            return reply; // not a pair array — leave the V2 wire alone
        }
        rest = &rest[4..];
        // copy exactly two bulk items
        for _ in 0..2 {
            let Some(le) = rest.iter().position(|&b| b == b'\n') else { return reply };
            if rest[0] != b'$' {
                return reply;
            }
            let Ok(len) = std::str::from_utf8(&rest[1..le - 1]).unwrap_or("x").parse::<usize>()
            else {
                return reply;
            };
            let total = le + 1 + len + 2;
            if rest.len() < total {
                return reply;
            }
            out.extend_from_slice(&rest[..total]);
            rest = &rest[total..];
        }
    }
    out.extend_from_slice(rest);
    out
}
