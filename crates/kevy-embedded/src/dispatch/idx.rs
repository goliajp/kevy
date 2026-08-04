//! IDX.* catalog verbs (CREATE / DROP / LIST) plus the value / cursor
//! wire codecs shared with `idx_query.rs` and `view.rs`. Grammar and
//! error wording mirror the server's `cmd_index.rs`; the reply shapes
//! mirror the origin reduces in `cmd_index_reduce`.

use crate::store::Store;

use kevy_index::{IndexSpec, IndexValue};

use super::util::{arr, bulk, err, int};

/// One IDX catalog request; `false` = verb not in this group (the
/// query shapes live in `idx_query.rs`).
pub(super) fn dispatch(s: &Store, up: &[u8], argv: &[Vec<u8>], out: &mut Vec<u8>) -> bool {
    match up {
        b"IDX.CREATE" => super::idx_create::cmd_idx_create(s, argv, out),
        b"IDX.DROP" => {
            if argv.len() != 2 {
                err(out, "ERR usage: IDX.DROP name");
            } else {
                int(out, i64::from(s.idx_drop(&argv[1])));
            }
        }
        b"IDX.LIST" => cmd_idx_list(s, out),
        b"IDX.ADVISE" => {
            if argv.len() != 1 {
                err(out, "ERR usage: IDX.ADVISE");
            } else {
                cmd_idx_advise(s, out);
            }
        }
        _ => return false,
    }
    true
}

/// `IDX.ADVISE` — `[count, name, advice]` rows matching the server's
/// Local handler: refusal families most-refused first, then the
/// never-hit drop suggestions.
fn cmd_idx_advise(s: &Store, out: &mut Vec<u8>) {
    let rows = s.idx_advise();
    arr(out, rows.len());
    for r in rows {
        arr(out, 3);
        int(out, r.count as i64);
        bulk(out, &r.name);
        bulk(out, r.advice.as_bytes());
    }
}

// ---- shared codecs (server `cmd_index_query::wire` shapes) -----------

pub(super) fn enc_value(out: &mut Vec<u8>, v: &IndexValue) {
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

pub(super) fn dec_value(b: &[u8], pos: &mut usize) -> Option<IndexValue> {
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

pub(super) fn hex(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len() * 2);
    for x in b {
        out.extend_from_slice(format!("{x:02x}").as_bytes());
    }
    out
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

/// Hex `(value, key)` cursor — the resume point every paged reply carries.
pub(super) fn encode_cursor(v: &IndexValue, k: &[u8]) -> Vec<u8> {
    let mut payload = Vec::new();
    enc_value(&mut payload, v);
    payload.extend_from_slice(k);
    hex(&payload)
}

pub(super) fn decode_cursor(raw: &[u8]) -> Option<(IndexValue, Vec<u8>)> {
    let bytes = unhex(raw)?;
    let mut pos = 0usize;
    let value = dec_value(&bytes, &mut pos)?;
    let key = bytes.get(pos..)?.to_vec();
    Some((value, key))
}

/// The wire text a hit's value renders as (server `value_repr`).
pub(super) fn value_repr(v: &IndexValue) -> Vec<u8> {
    match v {
        IndexValue::I64(i) => i.to_string().into_bytes(),
        IndexValue::F64(f) => format!("{f}").into_bytes(),
        IndexValue::Str(s) => s.clone(),
    }
}

/// Snapshot one declared index's spec from the embedded catalog.
pub(super) fn spec_of(s: &Store, name: &[u8]) -> Option<IndexSpec> {
    let g = s.indexes.catalog.read().unwrap_or_else(std::sync::PoisonError::into_inner);
    g.1.get(name).map(|(spec, _)| spec.clone())
}

pub(super) fn no_such_index(out: &mut Vec<u8>, name: &[u8]) {
    let n = String::from_utf8_lossy(name);
    err(out, &format!("ERR no such index '{n}' (IDX.LIST enumerates them)"));
}

pub(super) fn badargs(out: &mut Vec<u8>, verb: &str, name: &[u8]) {
    let n = String::from_utf8_lossy(name);
    err(out, &format!("ERR {verb} '{n}': bad arguments — run COMMAND DOCS {verb} for the syntax"));
}

/// `IDX.LIST` — 16-field rows matching the server's reduce. Embedded
/// builds are synchronous, so `state` is always `ready`; entry/byte
/// stats are the scalar-segment sums (kind-specific stats stay 0);
/// hits/last_hit read the usage dual.
fn cmd_idx_list(s: &Store, out: &mut Vec<u8>) {
    let specs: Vec<IndexSpec> = {
        let g = s.indexes.catalog.read().unwrap_or_else(std::sync::PoisonError::into_inner);
        g.1.iter().map(|(spec, _)| spec.clone()).collect()
    };
    arr(out, specs.len());
    for spec in &specs {
        let stats = s.idx_stats(&spec.name).unwrap_or_default();
        let (hits, last, _) = s.idx_usage(&spec.name).unwrap_or((0, 0, 0));
        arr(out, 16);
        bulk(out, b"name");
        bulk(out, &spec.name);
        bulk(out, b"prefix");
        bulk(out, &spec.prefix);
        bulk(out, b"kind");
        bulk(out, spec.kind.tag().as_bytes());
        bulk(out, b"state");
        bulk(out, b"ready");
        bulk(out, b"entries");
        bulk(out, stats.entries.to_string().as_bytes());
        bulk(out, b"bytes");
        bulk(out, stats.approx_bytes.to_string().as_bytes());
        bulk(out, b"hits");
        bulk(out, hits.to_string().as_bytes());
        bulk(out, b"last_hit");
        bulk(out, last.to_string().as_bytes());
    }
}
