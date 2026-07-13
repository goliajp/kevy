//! IDX.* catalog verbs (CREATE / DROP / LIST) plus the value / cursor
//! wire codecs shared with `idx_query.rs` and `view.rs`. Grammar and
//! error wording mirror the server's `cmd_index.rs`; the reply shapes
//! mirror the origin reduces in `cmd_index_reduce`.

use crate::store::Store;

use kevy_index::{IndexKind, IndexSpec, IndexValue, ValType};

use super::util::{arr, bulk, err, int, kevy_err};

/// One IDX catalog request; `false` = verb not in this group (the
/// query shapes live in `idx_query.rs`).
pub(super) fn dispatch(s: &Store, up: &[u8], argv: &[Vec<u8>], out: &mut Vec<u8>) -> bool {
    match up {
        b"IDX.CREATE" => cmd_idx_create(s, argv, out),
        b"IDX.DROP" => {
            if argv.len() != 2 {
                err(out, "ERR usage: IDX.DROP name");
            } else {
                int(out, i64::from(s.idx_drop(&argv[1])));
            }
        }
        b"IDX.LIST" => cmd_idx_list(s, out),
        _ => return false,
    }
    true
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

// ---- IDX.CREATE -------------------------------------------------------

const CREATE_USAGE: &str = "ERR usage: IDX.CREATE name ON PREFIX p FIELD f TYPE i64|f64|str|vector KIND range|unique|text|ann [MAXMEM b] [DIM d] [DISTANCE c] [M m] [EF e]";

/// Optional IDX.CREATE tail, parsed out (MAXMEM is accepted and
/// ignored: the embedded build is synchronous, with no budget).
// Without the `vector` feature the ANN knobs are parsed for grammar
// parity but never read (KIND ann errors before reaching them).
#[cfg_attr(not(feature = "vector"), allow(dead_code))]
struct CreateOpts {
    dim: u32,
    m: u16,
    ef: u16,
    distance: u8,
    group_by: Option<Vec<u8>>,
}

/// The fixed `name ON PREFIX p FIELD f TYPE t KIND k` header.
fn create_header_ok(argv: &[Vec<u8>]) -> bool {
    argv.len() >= 11
        && argv.len() % 2 == 1
        && argv[2].eq_ignore_ascii_case(b"ON")
        && argv[3].eq_ignore_ascii_case(b"PREFIX")
        && argv[5].eq_ignore_ascii_case(b"FIELD")
        && argv[7].eq_ignore_ascii_case(b"TYPE")
        && argv[9].eq_ignore_ascii_case(b"KIND")
}

fn cmd_idx_create(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    if !create_header_ok(argv) {
        return err(out, CREATE_USAGE);
    }
    let opts = match parse_create_opts(argv) {
        Ok(o) => o,
        Err(msg) => return err(out, msg),
    };
    let Some(ty) = ValType::parse(&argv[8]) else {
        return err(out, "ERR TYPE must be i64|f64|str|vector");
    };
    let Some(kind) = IndexKind::parse(&argv[10]) else {
        return err(out, "ERR KIND must be range|unique|text|ann");
    };
    if argv[4].is_empty() {
        return err(out, "ERR PREFIX must be non-empty");
    }
    if let Err(msg) = validate_kind_combo(kind, ty, &opts) {
        return err(out, msg);
    }
    let (name, prefix, field) = (&argv[1], &argv[4], &argv[6]);
    let res = match kind {
        IndexKind::Agg => {
            s.idx_create_agg(name, prefix, field, ty, opts.group_by.as_deref().unwrap_or(b""))
        }
        #[cfg(feature = "vector")]
        IndexKind::Ann => s.idx_create_ann(
            name,
            prefix,
            field,
            kevy_index::AnnSpec { dim: opts.dim, distance: opts.distance, m: opts.m, ef: opts.ef },
        ),
        #[cfg(not(feature = "vector"))]
        IndexKind::Ann => {
            return err(out, "ERR vector indexes need the `vector` feature");
        }
        _ => s.idx_create(name, prefix, field, ty, kind),
    };
    match res {
        Ok(()) => out.extend_from_slice(b"+OK\r\n"),
        Err(e) => kevy_err(out, &e),
    }
}

/// Parse the optional key/value pairs from argv idx 11 on (server
/// `parse_create_opts`, same bounds + wording).
fn parse_create_opts(argv: &[Vec<u8>]) -> Result<CreateOpts, &'static str> {
    let (mut dim, mut m, mut ef) = (0u32, 16u16, 200u16);
    let mut distance = 0u8;
    let mut group_by: Option<Vec<u8>> = None;
    let mut i = 11;
    while i + 1 < argv.len() {
        let (opt, val) = (&argv[i], &argv[i + 1]);
        let parsed: Option<u64> = std::str::from_utf8(val).ok().and_then(|s| s.parse().ok());
        if opt.eq_ignore_ascii_case(b"MAXMEM") {
            // Validated, then ignored: no build budget embedded.
            parsed.ok_or("ERR MAXMEM must be an integer byte count")?;
        } else if opt.eq_ignore_ascii_case(b"DIM") {
            match parsed {
                Some(v) if (1..=65_536).contains(&v) => dim = v as u32,
                _ => return Err("ERR DIM must be 1-65536"),
            }
        } else if opt.eq_ignore_ascii_case(b"M") {
            match parsed {
                Some(v) if (4..=64).contains(&v) => m = v as u16,
                _ => return Err("ERR M must be 4-64"),
            }
        } else if opt.eq_ignore_ascii_case(b"EF") {
            match parsed {
                Some(v) if (16..=1024).contains(&v) => ef = v as u16,
                _ => return Err("ERR EF must be 16-1024"),
            }
        } else if opt.eq_ignore_ascii_case(b"GROUPBY") {
            if val.is_empty() {
                return Err("ERR GROUPBY requires a field");
            }
            group_by = Some(val.to_vec());
        } else if opt.eq_ignore_ascii_case(b"DISTANCE") {
            distance = if val.eq_ignore_ascii_case(b"cosine") {
                0
            } else if val.eq_ignore_ascii_case(b"l2") {
                1
            } else if val.eq_ignore_ascii_case(b"ip") {
                2
            } else {
                return Err("ERR DISTANCE must be cosine|l2|ip");
            };
        } else {
            return Err("ERR syntax error");
        }
        i += 2;
    }
    Ok(CreateOpts { dim, m, ef, distance, group_by })
}

/// KIND × TYPE (× GROUPBY) compatibility, mirroring the server.
fn validate_kind_combo(
    kind: IndexKind,
    ty: ValType,
    opts: &CreateOpts,
) -> Result<(), &'static str> {
    match (kind, ty) {
        (IndexKind::Ann, ValType::Vector) if opts.dim > 0 => {}
        (IndexKind::Ann, _) => return Err("ERR KIND ann requires TYPE vector and DIM"),
        (_, ValType::Vector) => return Err("ERR TYPE vector requires KIND ann"),
        _ => {}
    }
    match (kind, &opts.group_by, ty) {
        (IndexKind::Agg, None, _) => Err("ERR KIND agg requires GROUPBY <field>"),
        (IndexKind::Agg, Some(_), ValType::Str | ValType::Vector) => {
            Err("ERR KIND agg requires TYPE i64|f64")
        }
        (k, Some(_), _) if k != IndexKind::Agg => Err("ERR GROUPBY requires KIND agg"),
        _ => Ok(()),
    }
}

/// `IDX.LIST` — 12-field rows matching the server's reduce. Embedded
/// builds are synchronous, so `state` is always `ready`; entry/byte
/// stats are the scalar-segment sums (kind-specific stats stay 0).
fn cmd_idx_list(s: &Store, out: &mut Vec<u8>) {
    let specs: Vec<IndexSpec> = {
        let g = s.indexes.catalog.read().unwrap_or_else(std::sync::PoisonError::into_inner);
        g.1.iter().map(|(spec, _)| spec.clone()).collect()
    };
    arr(out, specs.len());
    for spec in &specs {
        let stats = s.idx_stats(&spec.name).unwrap_or_default();
        arr(out, 12);
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
    }
}
