//! v2.5 — IDX.* read surface: the extension fan-out halves
//! (per-shard op + origin reduce) and the query grammar. Split from
//! [`crate::cmd_index`] under the 500-LOC house rule.

use kevy_index::{Cursor, IndexValue, ValType};
use kevy_resp::{encode_array_len, encode_bulk, encode_error, encode_integer};
use kevy_store::Store;

use crate::index_runtime;

/// One hit's hydrated field values (None = field absent).
type Hydrated = Vec<Option<Vec<u8>>>;

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
    if argv.get(1).is_some_and(|a| a.eq_ignore_ascii_case(b"COMPOSE")) {
        return op_compose(store, argv);
    }
    let Some(q) = Query::parse(argv) else {
        return vec![ST_BADARGS];
    };
    let res = index_runtime::with_ready_segment(store, &q.name, |spec, seg| match q.shape {
        Shape::Range { .. } | Shape::Eq { .. } => {
            let Some((min, max)) = q.bounds(spec.ty) else {
                return HitsOrChunk::Chunk(vec![ST_BADARGS]);
            };
            if verb.eq_ignore_ascii_case(b"IDX.COUNT") {
                let mut chunk = vec![ST_OK];
                chunk.extend_from_slice(&seg.count(&min, &max).to_le_bytes());
                return HitsOrChunk::Chunk(chunk);
            }
            let cursor = q.cursor(spec.ty);
            let (hits, _) = seg.range(&min, &max, cursor.as_ref(), q.limit);
            HitsOrChunk::Hits(hits)
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
            HitsOrChunk::Chunk(chunk)
        }
    });
    match res {
        Ok(HitsOrChunk::Chunk(chunk)) => chunk,
        Ok(HitsOrChunk::Hits(hits)) => {
            // Hydration happens OUTSIDE the segment borrow: the hits'
            // rows live on this shard, plain hash reads.
            let mut chunk = vec![ST_OK];
            chunk.extend_from_slice(&(hits.len() as u32).to_le_bytes());
            for (k, v) in &hits {
                chunk.extend_from_slice(&(k.len() as u32).to_le_bytes());
                chunk.extend_from_slice(k);
                encode_value(&mut chunk, v);
                encode_hydration(store, &mut chunk, k, &q.fields);
            }
            chunk
        }
        Err(e) if e.starts_with("INDEXBUILDING") => vec![ST_BUILDING],
        Err(_) => vec![ST_NOINDEX],
    }
}

enum HitsOrChunk {
    Hits(Vec<(Vec<u8>, IndexValue)>),
    Chunk(Vec<u8>),
}

/// Per-shard COMPOSE: both sub-queries run against THIS shard's
/// segments (a key lives on exactly one shard, so per-shard set
/// algebra composes globally). Key-ordered; cursor = key point.
fn op_compose(store: &mut Store, argv: &[Vec<u8>]) -> Vec<u8> {
    let Some(cq) = ComposeQuery::parse(argv) else {
        return vec![ST_BADARGS];
    };
    let res = index_runtime::with_two_ready_segments(
        store,
        &cq.a.name,
        &cq.b.name,
        |spec_a, seg_a, spec_b, seg_b| {
            let (min_a, max_a) = sub_bounds(&cq.a.shape, spec_a.ty)?;
            let (min_b, max_b) = sub_bounds(&cq.b.shape, spec_b.ty)?;
            let (a_hits, _) = seg_a.range(&min_a, &max_a, None, usize::MAX);
            let mut keys: Vec<Vec<u8>> = if cq.and {
                a_hits
                    .into_iter()
                    .filter(|(k, _)| {
                        seg_b
                            .verify_entry(k)
                            .is_some_and(|v| *v >= min_b && *v <= max_b)
                    })
                    .map(|(k, _)| k)
                    .collect()
            } else {
                let (b_hits, _) = seg_b.range(&min_b, &max_b, None, usize::MAX);
                let mut all: Vec<Vec<u8>> =
                    a_hits.into_iter().chain(b_hits).map(|(k, _)| k).collect();
                all.sort();
                all.dedup();
                all
            };
            keys.sort();
            if let Some(cur) = &cq.cursor_key {
                keys.retain(|k| k.as_slice() > cur.as_slice());
            }
            keys.truncate(cq.limit);
            Some(keys)
        },
    );
    match res {
        Ok(Some(keys)) => {
            let mut chunk = vec![ST_OK];
            chunk.extend_from_slice(&(keys.len() as u32).to_le_bytes());
            for k in &keys {
                chunk.extend_from_slice(&(k.len() as u32).to_le_bytes());
                chunk.extend_from_slice(k);
                encode_hydration(store, &mut chunk, k, &cq.fields);
            }
            chunk
        }
        Ok(None) => vec![ST_BADARGS],
        Err(e) if e.starts_with("INDEXBUILDING") => vec![ST_BUILDING],
        Err(_) => vec![ST_NOINDEX],
    }
}

/// Append `[fcount u8][(flen u32|MAX=nil, bytes)*]` for the FIELDS
/// hydration list (owning-shard hash reads).
fn encode_hydration(store: &mut Store, chunk: &mut Vec<u8>, key: &[u8], fields: &[Vec<u8>]) {
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
    // IDX.QUERY COMPOSE: merge key-ordered chunks.
    if argv.get(1).is_some_and(|a| a.eq_ignore_ascii_case(b"COMPOSE")) {
        let Some(cq) = ComposeQuery::parse(argv) else {
            encode_error(&mut out, "ERR bad IDX arguments");
            return out;
        };
        let mut all: Vec<(Vec<u8>, Hydrated)> = Vec::new();
        for c in &chunks {
            let mut pos = 1usize;
            let Some(n) = read_u32(c, &mut pos) else { continue };
            for _ in 0..n {
                let Some(key) = read_kbytes(c, &mut pos) else { break };
                let Some(fv) = read_hydration(c, &mut pos) else { break };
                all.push((key, fv));
            }
        }
        all.sort_by(|a, b| a.0.cmp(&b.0));
        all.truncate(cq.limit);
        let next = if all.len() == cq.limit {
            all.last().map(|(k, _)| hex(k)).unwrap_or_else(|| b"0".to_vec())
        } else {
            b"0".to_vec()
        };
        encode_array_len(&mut out, 2);
        encode_bulk(&mut out, &next);
        encode_array_len(&mut out, all.len() as i64);
        for (k, fv) in &all {
            emit_row(&mut out, k, None, fv, &cq.fields);
        }
        return out;
    }
    // IDX.QUERY: k-way merge by (value, key), global LIMIT + cursor.
    let Some(q) = Query::parse(argv) else {
        encode_error(&mut out, "ERR bad IDX arguments");
        return out;
    };
    let mut all: Vec<(IndexValue, Vec<u8>, Hydrated)> = Vec::new();
    for c in &chunks {
        let mut pos = 1usize;
        let Some(n) = read_u32(c, &mut pos) else { continue };
        for _ in 0..n {
            let Some(key) = read_kbytes(c, &mut pos) else { break };
            let Some(v) = decode_value(c, &mut pos) else { break };
            let Some(fv) = read_hydration(c, &mut pos) else { break };
            all.push((v, key, fv));
        }
    }
    all.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    all.truncate(q.limit);
    let next = if all.len() == q.limit {
        all.last().map(|(v, k, _)| encode_cursor(v, k)).unwrap_or_else(|| b"0".to_vec())
    } else {
        b"0".to_vec()
    };
    encode_array_len(&mut out, 2);
    encode_bulk(&mut out, &next);
    if q.fields.is_empty() {
        // legacy flat shape: *2N of key/value
        encode_array_len(&mut out, (all.len() * 2) as i64);
        for (v, k, _) in &all {
            encode_bulk(&mut out, k);
            encode_bulk(&mut out, &value_repr(v));
        }
    } else {
        encode_array_len(&mut out, all.len() as i64);
        for (v, k, fv) in &all {
            emit_row(&mut out, k, Some(v), fv, &q.fields);
        }
    }
    out
}

fn read_u32(c: &[u8], pos: &mut usize) -> Option<u32> {
    let v = u32::from_le_bytes(c.get(*pos..*pos + 4)?.try_into().ok()?);
    *pos += 4;
    Some(v)
}

fn read_kbytes(c: &[u8], pos: &mut usize) -> Option<Vec<u8>> {
    let n = read_u32(c, pos)? as usize;
    let b = c.get(*pos..*pos + n)?.to_vec();
    *pos += n;
    Some(b)
}

fn read_hydration(c: &[u8], pos: &mut usize) -> Option<Hydrated> {
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
fn emit_row(
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
    hex(&payload)
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

/// One side of a COMPOSE (name + its shape).
struct SubQuery {
    name: Vec<u8>,
    shape: Shape,
}

struct Query {
    name: Vec<u8>,
    shape: Shape,
    limit: usize,
    cursor_raw: Option<Vec<u8>>,
    /// `FIELDS f…` hydration list (owning-shard hash reads ride the
    /// chunk; empty = keys/values only).
    fields: Vec<Vec<u8>>,
}

/// `IDX.QUERY COMPOSE AND|OR sub1 sub2 …` — key-ordered (the two
/// indexes' value domains differ, so composition orders by key and
/// the cursor is a plain key point).
struct ComposeQuery {
    and: bool,
    a: SubQuery,
    b: SubQuery,
    limit: usize,
    cursor_key: Option<Vec<u8>>,
    fields: Vec<Vec<u8>>,
}

impl ComposeQuery {
    /// `IDX.QUERY COMPOSE AND|OR nameA <shapeA> nameB <shapeB>
    /// [LIMIT n] [CURSOR k] [FIELDS f…]` where shape =
    /// `RANGE min max` | `EQ v`.
    fn parse(argv: &[Vec<u8>]) -> Option<ComposeQuery> {
        if !argv.first()?.eq_ignore_ascii_case(b"IDX.QUERY")
            || !argv.get(1)?.eq_ignore_ascii_case(b"COMPOSE")
        {
            return None;
        }
        let mode = argv.get(2)?;
        let and = if mode.eq_ignore_ascii_case(b"AND") {
            true
        } else if mode.eq_ignore_ascii_case(b"OR") {
            false
        } else {
            return None;
        };
        let (a, i) = parse_sub(argv, 3)?;
        let (b, mut i) = parse_sub(argv, i)?;
        let mut limit = 100usize;
        let mut cursor_key = None;
        let mut fields = Vec::new();
        while i < argv.len() {
            let t = &argv[i];
            if t.eq_ignore_ascii_case(b"LIMIT") {
                limit = std::str::from_utf8(argv.get(i + 1)?).ok()?.parse().ok()?;
                i += 2;
            } else if t.eq_ignore_ascii_case(b"CURSOR") {
                let raw = argv.get(i + 1)?;
                cursor_key = if raw == b"0" { None } else { Some(unhex(raw)?) };
                i += 2;
            } else if t.eq_ignore_ascii_case(b"FIELDS") {
                fields = argv[i + 1..].to_vec();
                if fields.is_empty() {
                    return None;
                }
                break;
            } else {
                return None;
            }
        }
        Some(ComposeQuery { and, a, b, limit: limit.clamp(1, 10_000), cursor_key, fields })
    }
}

fn parse_sub(argv: &[Vec<u8>], i: usize) -> Option<(SubQuery, usize)> {
    let name = argv.get(i)?.clone();
    let mode = argv.get(i + 1)?;
    if mode.eq_ignore_ascii_case(b"RANGE") {
        Some((
            SubQuery {
                name,
                shape: Shape::Range { min: argv.get(i + 2)?.clone(), max: argv.get(i + 3)?.clone() },
            },
            i + 4,
        ))
    } else if mode.eq_ignore_ascii_case(b"EQ") {
        Some((SubQuery { name, shape: Shape::Eq { value: argv.get(i + 2)?.clone() } }, i + 3))
    } else {
        None
    }
}

fn sub_bounds(shape: &Shape, ty: ValType) -> Option<(IndexValue, IndexValue)> {
    match shape {
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

fn unhex(raw: &[u8]) -> Option<Vec<u8>> {
    if !raw.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(raw.len() / 2);
    for pair in raw.chunks(2) {
        out.push(u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?);
    }
    Some(out)
}

fn hex(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len() * 2);
    for x in b {
        out.extend_from_slice(format!("{x:02x}").as_bytes());
    }
    out
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
                fields: Vec::new(),
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
        let mut fields = Vec::new();
        while i < argv.len() {
            let a = &argv[i];
            if a.eq_ignore_ascii_case(b"LIMIT") {
                limit = std::str::from_utf8(argv.get(i + 1)?).ok()?.parse().ok()?;
                i += 2;
            } else if a.eq_ignore_ascii_case(b"CURSOR") {
                cursor_raw = Some(argv.get(i + 1)?.clone());
                i += 2;
            } else if a.eq_ignore_ascii_case(b"FIELDS") {
                fields = argv[i + 1..].to_vec();
                if fields.is_empty() {
                    return None;
                }
                break;
            } else {
                return None;
            }
        }
        Some(Query { name, shape, limit: limit.clamp(1, 10_000), cursor_raw, fields })
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
