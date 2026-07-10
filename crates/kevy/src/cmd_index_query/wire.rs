//! Chunk / cursor wire encoding shared by the per-shard ops and the
//! origin reduce.

use kevy_index::{Cursor, IndexValue};
use kevy_store::Store;

use super::ST_OK;

pub(crate) fn encode_value(out: &mut Vec<u8>, v: &IndexValue) {
    match v {
        IndexValue::I64(i) => {
            out.push(0);
            out.extend_from_slice(&i.to_le_bytes());
        }
        IndexValue::F64(f) => {
            out.push(1);
            out.extend_from_slice(&f.to_le_bytes());
        }
        IndexValue::Str(s) => {
            out.push(2);
            out.extend_from_slice(&(s.len() as u32).to_le_bytes());
            out.extend_from_slice(s);
        }
    }
}

pub(crate) fn decode_value(b: &[u8], pos: &mut usize) -> Option<IndexValue> {
    let tag = *b.get(*pos)?;
    *pos += 1;
    match tag {
        0 => {
            let v = i64::from_le_bytes(b.get(*pos..*pos + 8)?.try_into().ok()?);
            *pos += 8;
            Some(IndexValue::I64(v))
        }
        1 => {
            let v = f64::from_le_bytes(b.get(*pos..*pos + 8)?.try_into().ok()?);
            *pos += 8;
            Some(IndexValue::F64(v))
        }
        2 => {
            let n = u32::from_le_bytes(b.get(*pos..*pos + 4)?.try_into().ok()?) as usize;
            *pos += 4;
            let s = b.get(*pos..*pos + n)?.to_vec();
            *pos += n;
            Some(IndexValue::Str(s))
        }
        _ => None,
    }
}

pub(crate) fn decode_view_cursor(raw: &[u8]) -> Option<(IndexValue, Vec<u8>)> {
    decode_cursor(raw).map(|c| (c.value, c.key))
}

pub(super) fn decode_cursor(raw: &[u8]) -> Option<Cursor> {
    if raw == b"0" {
        return None;
    }
    if !raw.len().is_multiple_of(2) {
        return None;
    }
    let mut bytes = Vec::with_capacity(raw.len() / 2);
    for pair in raw.chunks(2) {
        let s = std::str::from_utf8(pair).ok()?;
        bytes.push(u8::from_str_radix(s, 16).ok()?);
    }
    let mut pos = 0usize;
    let value = decode_value(&bytes, &mut pos)?;
    let key = bytes.get(pos..)?.to_vec();
    Some(Cursor { value, key })
}

pub(super) fn unhex(raw: &[u8]) -> Option<Vec<u8>> {
    if !raw.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(raw.len() / 2);
    for pair in raw.chunks(2) {
        out.push(u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?);
    }
    Some(out)
}

pub(crate) fn hex(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len() * 2);
    for x in b {
        out.extend_from_slice(format!("{x:02x}").as_bytes());
    }
    out
}

/// Append `[fcount u8][(flen u32|MAX=nil, bytes)*]` for the FIELDS
/// hydration list (owning-shard hash reads).
pub(super) fn encode_hydration(store: &mut Store, chunk: &mut Vec<u8>, key: &[u8], fields: &[Vec<u8>]) {
    chunk.push(fields.len() as u8);
    for f in fields {
        match store.hget(key, f) {
            Ok(Some(v)) => {
                let v = v.to_vec();
                chunk.extend_from_slice(&(v.len() as u32).to_le_bytes());
                chunk.extend_from_slice(&v);
            }
            _ => chunk.extend_from_slice(&u32::MAX.to_le_bytes()),
        }
    }
}

/// Shared agg chunk encoding: `[ST_OK][n][(glen,g,count,sum,mmlen,mm)*]`.
pub(super) fn encode_agg_chunk(rows: &[(Vec<u8>, kevy_index::GroupStats)]) -> Vec<u8> {
    let mut chunk = vec![ST_OK];
    chunk.extend_from_slice(&(rows.len() as u32).to_le_bytes());
    for (g, st) in rows {
        chunk.extend_from_slice(&(g.len() as u32).to_le_bytes());
        chunk.extend_from_slice(g);
        chunk.extend_from_slice(&st.count.to_le_bytes());
        chunk.extend_from_slice(&st.sum.to_le_bytes());
        let mut mm = Vec::new();
        for v in [&st.min, &st.max] {
            match v {
                Some(x) => {
                    mm.push(1);
                    encode_value(&mut mm, x);
                }
                None => mm.push(0),
            }
        }
        chunk.extend_from_slice(&(mm.len() as u32).to_le_bytes());
        chunk.extend_from_slice(&mm);
    }
    chunk
}
