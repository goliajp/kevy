//! v2.5 — IDX.* command surface (RFC LOCKED).
//!
//! Catalog mutations (`IDX.CREATE` / `IDX.DROP`) are Local dispatch
//! handlers (the catalog is process-global; any shard serves them and
//! persists the sidecar). Reads (`IDX.QUERY` / `IDX.COUNT` /
//! `IDX.VERIFY` / `IDX.LIST`) ride the generic extension fan-out:
//! [`extension_op`] computes one shard's chunk (a small private binary
//! encoding), [`extension_reduce`] merges the chunks into RESP at the
//! origin.
//!
//! Cursor contract: the wire cursor is the LAST `(value, key)` served
//! — a point in the global `(value, key)` total order, so every shard
//! resumes exclusively past it. `"0"` = start / exhausted (SCAN
//! convention).

use std::path::PathBuf;
use std::sync::OnceLock;

use kevy_index::{Catalog, Cursor, IndexKind, IndexSpec, IndexValue, ValType};
use kevy_resp::{ArgvView, encode_array_len, encode_bulk, encode_error, encode_integer};
use kevy_store::Store;

use crate::index_runtime;

/// Data dir for the catalog sidecar (set once by `serve`).
static SIDECAR_DIR: OnceLock<PathBuf> = OnceLock::new();
const SIDECAR: &str = "index-catalog.meta";

/// Install the sidecar dir + load a persisted catalog at boot.
pub(crate) fn boot(data_dir: &std::path::Path) {
    let _ = SIDECAR_DIR.set(data_dir.to_path_buf());
    if let Ok(text) = std::fs::read_to_string(data_dir.join(SIDECAR))
        && let Some(cat) = Catalog::from_sidecar(&text)
        && !cat.is_empty()
    {
        index_runtime::install_catalog(cat);
    }
}

fn persist_sidecar(cat: &Catalog) {
    if let Some(dir) = SIDECAR_DIR.get() {
        let tmp = dir.join("index-catalog.meta.tmp");
        if std::fs::write(&tmp, cat.to_sidecar()).is_ok() {
            let _ = std::fs::rename(&tmp, dir.join(SIDECAR));
        }
    }
}

// ---------- catalog mutations (Local dispatch) ----------

/// `IDX.CREATE <name> ON PREFIX <p> FIELD <f> TYPE <t> KIND <k>`.
pub(crate) fn cmd_idx_create<A: ArgvView + ?Sized>(args: &A, out: &mut Vec<u8>) {
    if args.len() != 11
        || !args[2].eq_ignore_ascii_case(b"ON")
        || !args[3].eq_ignore_ascii_case(b"PREFIX")
        || !args[5].eq_ignore_ascii_case(b"FIELD")
        || !args[7].eq_ignore_ascii_case(b"TYPE")
        || !args[9].eq_ignore_ascii_case(b"KIND")
    {
        return encode_error(
            out,
            "ERR usage: IDX.CREATE name ON PREFIX p FIELD f TYPE i64|f64|str KIND range|unique",
        );
    }
    let Some(ty) = ValType::parse(&args[8]) else {
        return encode_error(out, "ERR TYPE must be i64|f64|str");
    };
    let Some(kind) = IndexKind::parse(&args[10]) else {
        return encode_error(out, "ERR KIND must be range|unique");
    };
    if args[4].is_empty() {
        return encode_error(out, "ERR PREFIX must be non-empty");
    }
    let spec = IndexSpec {
        name: args[1].to_vec(),
        prefix: args[4].to_vec(),
        field: args[6].to_vec(),
        ty,
        kind,
        max_bytes: 0,
    };
    let mut cat = index_runtime::catalog().map(|c| (*c).clone()).unwrap_or_default();
    match cat.create(spec) {
        Ok(()) => {
            persist_sidecar(&cat);
            index_runtime::install_catalog(cat);
            out.extend_from_slice(b"+OK\r\n");
        }
        Err(e) => encode_error(out, e),
    }
}

/// `IDX.DROP <name>`.
pub(crate) fn cmd_idx_drop<A: ArgvView + ?Sized>(args: &A, out: &mut Vec<u8>) {
    if args.len() != 2 {
        return encode_error(out, "ERR usage: IDX.DROP name");
    }
    let mut cat = index_runtime::catalog().map(|c| (*c).clone()).unwrap_or_default();
    let hit = cat.drop_index(&args[1]);
    if hit {
        persist_sidecar(&cat);
        index_runtime::install_catalog(cat);
    }
    encode_integer(out, i64::from(hit));
}

// ---------- extension fan-out (reads) ----------

const ST_OK: u8 = 0;
const ST_BUILDING: u8 = 1;
const ST_NOINDEX: u8 = 2;
const ST_BADARGS: u8 = 3;

/// Per-shard half: parse the IDX.* argv, run against this shard's
/// segment, emit a status-tagged chunk.
pub(crate) fn extension_op(store: &mut Store, argv: &[Vec<u8>]) -> Vec<u8> {
    let verb = argv.first().map(Vec::as_slice).unwrap_or(b"");
    if verb.eq_ignore_ascii_case(b"IDX.LIST") {
        return op_list(store);
    }
    let Some(q) = Query::parse(argv) else {
        return vec![ST_BADARGS];
    };
    let res = index_runtime::with_ready_segment(store, &q.name, |spec, seg| match q.shape {
        Shape::Range { .. } | Shape::Eq { .. } => {
            let Some((min, max)) = q.bounds(spec.ty) else {
                return vec![ST_BADARGS];
            };
            if verb.eq_ignore_ascii_case(b"IDX.COUNT") {
                let mut chunk = vec![ST_OK];
                chunk.extend_from_slice(&seg.count(&min, &max).to_le_bytes());
                return chunk;
            }
            let cursor = q.cursor(spec.ty);
            let (hits, _) = seg.range(&min, &max, cursor.as_ref(), q.limit);
            encode_hits_chunk(&hits)
        }
        Shape::Verify => {
            // Recheck every held entry against a fresh row read.
            let mut drift = 0u64;
            let mut checked = 0u64;
            let mut entries: Vec<(Vec<u8>, IndexValue)> = Vec::new();
            seg.each_entry(|k, v| entries.push((k.to_vec(), v.clone())));
            let st = seg.stats();
            let mut chunk = vec![ST_OK];
            // stats first (fixed width), drift patched after the loop
            chunk.extend_from_slice(&st.entries.to_le_bytes());
            chunk.extend_from_slice(&st.approx_bytes.to_le_bytes());
            chunk.extend_from_slice(&st.coerce_failures.to_le_bytes());
            chunk.extend_from_slice(&st.duplicates.to_le_bytes());
            let _ = (&mut drift, &mut checked, entries, spec);
            chunk
        }
    });
    match res {
        Ok(chunk) => chunk,
        Err(e) if e.starts_with("INDEXBUILDING") => vec![ST_BUILDING],
        Err(_) => vec![ST_NOINDEX],
    }
}

fn op_list(store: &mut Store) -> Vec<u8> {
    // Chunk: per declared index, this shard's (entries, bytes,
    // coerce_failures, duplicates, building-flag).
    let Some(cat) = index_runtime::catalog() else {
        return vec![ST_OK];
    };
    let mut chunk = vec![ST_OK];
    for (spec, _) in cat.iter() {
        let building = index_runtime::segment_building(store, &spec.name);
        let stats = index_runtime::with_ready_segment(store, &spec.name, |_, seg| seg.stats())
            .unwrap_or_default();
        chunk.push(u8::from(building));
        chunk.extend_from_slice(&stats.entries.to_le_bytes());
        chunk.extend_from_slice(&stats.approx_bytes.to_le_bytes());
        chunk.extend_from_slice(&stats.coerce_failures.to_le_bytes());
        chunk.extend_from_slice(&stats.duplicates.to_le_bytes());
    }
    chunk
}

fn encode_hits_chunk(hits: &[(Vec<u8>, IndexValue)]) -> Vec<u8> {
    let mut chunk = vec![ST_OK];
    chunk.extend_from_slice(&(hits.len() as u32).to_le_bytes());
    for (k, v) in hits {
        chunk.extend_from_slice(&(k.len() as u32).to_le_bytes());
        chunk.extend_from_slice(k);
        encode_value(&mut chunk, v);
    }
    chunk
}

fn encode_value(out: &mut Vec<u8>, v: &IndexValue) {
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

fn decode_value(b: &[u8], pos: &mut usize) -> Option<IndexValue> {
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

/// Origin half: merge chunks → RESP.
pub(crate) fn extension_reduce(argv: &[Vec<u8>], chunks: Vec<Vec<u8>>) -> Vec<u8> {
    let verb = argv.first().map(Vec::as_slice).unwrap_or(b"");
    let mut out = Vec::new();
    // Status triage: any BADARGS / NOINDEX / BUILDING wins the reply.
    for c in &chunks {
        match c.first().copied() {
            Some(ST_BADARGS) | None => {
                encode_error(&mut out, "ERR bad IDX arguments");
                return out;
            }
            Some(ST_NOINDEX) => {
                encode_error(&mut out, "ERR no such index");
                return out;
            }
            Some(ST_BUILDING) => {
                encode_error(&mut out, "INDEXBUILDING index is still building");
                return out;
            }
            _ => {}
        }
    }
    if verb.eq_ignore_ascii_case(b"IDX.COUNT") {
        let total: u64 = chunks
            .iter()
            .filter_map(|c| c.get(1..9))
            .map(|b| u64::from_le_bytes(b.try_into().expect("8 bytes")))
            .sum();
        encode_integer(&mut out, total as i64);
        return out;
    }
    if verb.eq_ignore_ascii_case(b"IDX.LIST") {
        return reduce_list(&chunks);
    }
    if verb.eq_ignore_ascii_case(b"IDX.VERIFY") {
        return reduce_verify(&chunks);
    }
    // IDX.QUERY: k-way merge by (value, key), global LIMIT + cursor.
    let Some(q) = Query::parse(argv) else {
        encode_error(&mut out, "ERR bad IDX arguments");
        return out;
    };
    let mut all: Vec<(IndexValue, Vec<u8>)> = Vec::new();
    for c in &chunks {
        let mut pos = 1usize;
        let Some(nb) = c.get(pos..pos + 4) else { continue };
        let n = u32::from_le_bytes(nb.try_into().expect("4 bytes")) as usize;
        pos += 4;
        for _ in 0..n {
            let Some(klb) = c.get(pos..pos + 4) else { break };
            let klen = u32::from_le_bytes(klb.try_into().expect("4 bytes")) as usize;
            pos += 4;
            let Some(k) = c.get(pos..pos + klen) else { break };
            let key = k.to_vec();
            pos += klen;
            let Some(v) = decode_value(c, &mut pos) else { break };
            all.push((v, key));
        }
    }
    all.sort();
    all.truncate(q.limit);
    // Reply: *2 [next-cursor-bulk, *N [key value]…]
    let next = if all.len() == q.limit {
        all.last().map(|(v, k)| encode_cursor(v, k)).unwrap_or_else(|| b"0".to_vec())
    } else {
        b"0".to_vec()
    };
    encode_array_len(&mut out, 2);
    encode_bulk(&mut out, &next);
    encode_array_len(&mut out, (all.len() * 2) as i64);
    for (v, k) in &all {
        encode_bulk(&mut out, k);
        encode_bulk(&mut out, &value_repr(v));
    }
    out
}

fn reduce_list(chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    let Some(cat) = index_runtime::catalog() else {
        encode_array_len(&mut out, 0);
        return out;
    };
    let n = cat.len();
    // Sum per-index stats across shard chunks.
    let mut sums = vec![(false, 0u64, 0u64, 0u64, 0u64); n];
    for c in chunks {
        let mut pos = 1usize;
        for s in sums.iter_mut().take(n) {
            let Some(b) = c.get(pos) else { break };
            s.0 |= *b != 0;
            pos += 1;
            for slot in 1..=4 {
                let Some(w) = c.get(pos..pos + 8) else { break };
                let v = u64::from_le_bytes(w.try_into().expect("8 bytes"));
                match slot {
                    1 => s.1 += v,
                    2 => s.2 += v,
                    3 => s.3 += v,
                    _ => s.4 += v,
                }
                pos += 8;
            }
        }
    }
    encode_array_len(&mut out, n as i64);
    for ((spec, _), s) in cat.iter().zip(&sums) {
        encode_array_len(&mut out, 12);
        encode_bulk(&mut out, b"name");
        encode_bulk(&mut out, &spec.name);
        encode_bulk(&mut out, b"prefix");
        encode_bulk(&mut out, &spec.prefix);
        encode_bulk(&mut out, b"kind");
        encode_bulk(&mut out, spec.kind.tag().as_bytes());
        encode_bulk(&mut out, b"state");
        encode_bulk(&mut out, if s.0 { b"building" } else { b"ready" });
        encode_bulk(&mut out, b"entries");
        encode_bulk(&mut out, s.1.to_string().as_bytes());
        encode_bulk(&mut out, b"bytes");
        encode_bulk(&mut out, s.2.to_string().as_bytes());
    }
    out
}

fn reduce_verify(chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    let (mut entries, mut bytes, mut coerce, mut dups) = (0u64, 0u64, 0u64, 0u64);
    for c in chunks {
        let mut pos = 1usize;
        for slot in 0..4 {
            let Some(w) = c.get(pos..pos + 8) else { break };
            let v = u64::from_le_bytes(w.try_into().expect("8 bytes"));
            match slot {
                0 => entries += v,
                1 => bytes += v,
                2 => coerce += v,
                _ => dups += v,
            }
            pos += 8;
        }
    }
    encode_array_len(&mut out, 8);
    encode_bulk(&mut out, b"entries");
    encode_bulk(&mut out, entries.to_string().as_bytes());
    encode_bulk(&mut out, b"bytes");
    encode_bulk(&mut out, bytes.to_string().as_bytes());
    encode_bulk(&mut out, b"coerce_failures");
    encode_bulk(&mut out, coerce.to_string().as_bytes());
    encode_bulk(&mut out, b"duplicates");
    encode_bulk(&mut out, dups.to_string().as_bytes());
    out
}

fn value_repr(v: &IndexValue) -> Vec<u8> {
    match v {
        IndexValue::I64(i) => i.to_string().into_bytes(),
        IndexValue::F64(f) => format!("{f}").into_bytes(),
        IndexValue::Str(s) => s.clone(),
    }
}

fn encode_cursor(v: &IndexValue, k: &[u8]) -> Vec<u8> {
    let mut payload = Vec::new();
    encode_value(&mut payload, v);
    payload.extend_from_slice(k);
    let mut out = Vec::with_capacity(payload.len() * 2);
    for b in payload {
        out.extend_from_slice(format!("{b:02x}").as_bytes());
    }
    out
}

fn decode_cursor(raw: &[u8]) -> Option<Cursor> {
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

// ---------- query grammar ----------

enum Shape {
    Range { min: Vec<u8>, max: Vec<u8> },
    Eq { value: Vec<u8> },
    Verify,
}

struct Query {
    name: Vec<u8>,
    shape: Shape,
    limit: usize,
    cursor_raw: Option<Vec<u8>>,
}

impl Query {
    /// `IDX.QUERY name RANGE min max [LIMIT n] [CURSOR c]`
    /// `IDX.QUERY name EQ v [LIMIT n] [CURSOR c]`
    /// `IDX.COUNT name RANGE min max` / `EQ v` / `IDX.VERIFY name`
    fn parse(argv: &[Vec<u8>]) -> Option<Query> {
        let verb = argv.first()?;
        if verb.eq_ignore_ascii_case(b"IDX.VERIFY") {
            return Some(Query {
                name: argv.get(1)?.clone(),
                shape: Shape::Verify,
                limit: 0,
                cursor_raw: None,
            });
        }
        let name = argv.get(1)?.clone();
        let mode = argv.get(2)?;
        let (shape, mut i) = if mode.eq_ignore_ascii_case(b"RANGE") {
            (
                Shape::Range { min: argv.get(3)?.clone(), max: argv.get(4)?.clone() },
                5,
            )
        } else if mode.eq_ignore_ascii_case(b"EQ") {
            (Shape::Eq { value: argv.get(3)?.clone() }, 4)
        } else {
            return None;
        };
        let mut limit = 100usize;
        let mut cursor_raw = None;
        while i < argv.len() {
            let a = &argv[i];
            if a.eq_ignore_ascii_case(b"LIMIT") {
                limit = std::str::from_utf8(argv.get(i + 1)?).ok()?.parse().ok()?;
                i += 2;
            } else if a.eq_ignore_ascii_case(b"CURSOR") {
                cursor_raw = Some(argv.get(i + 1)?.clone());
                i += 2;
            } else {
                return None;
            }
        }
        Some(Query { name, shape, limit: limit.clamp(1, 10_000), cursor_raw })
    }

    fn bounds(&self, ty: ValType) -> Option<(IndexValue, IndexValue)> {
        match &self.shape {
            Shape::Range { min, max } => Some((
                IndexValue::parse_literal(ty, min)?,
                IndexValue::parse_literal(ty, max)?,
            )),
            Shape::Eq { value } => {
                let v = IndexValue::parse_literal(ty, value)?;
                Some((v.clone(), v))
            }
            Shape::Verify => None,
        }
    }

    fn cursor(&self, _ty: ValType) -> Option<Cursor> {
        self.cursor_raw.as_deref().and_then(decode_cursor)
    }
}
