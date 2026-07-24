//! Scalar / admin reduces: IDX.EXPLAIN, IDX.COUNT, IDX.LIST,
//! IDX.VERIFY, IDX.REBUILD, COMPOSE and the plain IDX.QUERY k-way
//! merge.

use kevy_index::IndexValue;
use kevy_resp::{encode_array_len, encode_bulk, encode_error, encode_integer};

use super::chunk::{emit_row, encode_cursor, read_hydration, read_kbytes, read_u32, value_repr};
use crate::cmd_index_query::{ComposeQuery, Hydrated, Query, decode_value, hex};
use crate::state::CatalogState;

/// IDX.EXPLAIN — pair-array plan summary; per-shard chunks
/// carry [ST_OK][building][entries u64][shape byte].
pub(super) fn reduce_explain(
    catalogs: &CatalogState,
    argv: &[Vec<u8>],
    chunks: &[Vec<u8>],
) -> Vec<u8> {
    let mut out = Vec::new();
    let (est_rows, building, shape_b) = fold_explain_chunks(chunks);
    let kind = catalogs
        .index()
        .and_then(|cat| {
            cat.iter()
                .map(|(s, _)| s)
                .find(|s| Some(s.name.as_slice()) == argv.get(1).map(Vec::as_slice))
                .map(|s| format!("{:?}", s.kind).to_ascii_lowercase())
        })
        .unwrap_or_else(|| "?".into());
    let shape = match shape_b {
        b'M' => "match",
        b'K' => "knn",
        b'G' => "groups",
        b'R' => "range",
        b'E' => "eq",
        _ => "query",
    };
    let state = if building { "building" } else { "ready" };
    let plan = format!(
        "single-index scan: kind={kind} shape={shape}, {} shard(s) fan-out, merge at origin",
        chunks.len()
    );
    encode_array_len(&mut out, 4);
    for (k, v) in [
        ("kind", kind.as_str()),
        ("state", state),
        ("est_rows", &est_rows.to_string()),
        ("plan", &plan),
    ] {
        encode_array_len(&mut out, 2);
        encode_bulk(&mut out, k.as_bytes());
        encode_bulk(&mut out, v.as_bytes());
    }
    out
}

/// Sum the per-shard EXPLAIN chunks: `(est_rows, building, shape byte)`.
fn fold_explain_chunks(chunks: &[Vec<u8>]) -> (u64, bool, u8) {
    let mut est_rows: u64 = 0;
    let mut building = false;
    let mut shape_b = b'?';
    for c in chunks {
        if c.len() >= 11 {
            building |= c[1] != 0;
            est_rows += u64::from_le_bytes(c[2..10].try_into().expect("8 bytes"));
            shape_b = c[10];
        }
    }
    (est_rows, building, shape_b)
}

pub(super) fn reduce_count(chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    let total: u64 = chunks
        .iter()
        .filter_map(|c| c.get(1..9))
        .map(|b| u64::from_le_bytes(b.try_into().expect("8 bytes")))
        .sum();
    encode_integer(&mut out, total as i64);
    out
}

/// REBUILD: all shards OK → +OK.
pub(super) fn reduce_rebuild(chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for c in chunks {
        match c.first().copied() {
            Some(x) if x == crate::cmd_index_query::ST_BUILDING => {
                encode_error(&mut out, "INDEXBUILDING index is still building");
                return out;
            }
            Some(x) if x == crate::cmd_index_query::ST_OK => {}
            _ => {
                encode_error(&mut out, "ERR no such vector index");
                return out;
            }
        }
    }
    out.extend_from_slice(b"+OK\r\n");
    out
}

/// IDX.QUERY COMPOSE: merge key-ordered chunks.
pub(super) fn reduce_compose(argv: &[Vec<u8>], chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    let Some(cq) = ComposeQuery::parse(argv) else {
        encode_error(&mut out, "ERR bad IDX arguments");
        return out;
    };
    let mut all: Vec<(Vec<u8>, Hydrated)> = Vec::new();
    for c in chunks {
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
    out
}

/// IDX.QUERY: k-way merge by (value, key), global LIMIT + cursor.
/// Selection clauses (SORT / DISTINCT / FACET / OFFSET) take the
/// claused reduce; FILTER alone rides this path unchanged — the shards
/// already thinned their pages, and the global cursor still works.
pub(super) fn reduce_query(argv: &[Vec<u8>], chunks: &[Vec<u8>]) -> Vec<u8> {
    let Some(q) = Query::parse(argv) else {
        return super::claused::bad_args();
    };
    if q.selects() {
        return super::claused::reduce_query_claused(&q, chunks);
    }
    let mut out = Vec::new();
    let mut all: Vec<(IndexValue, Vec<u8>, Hydrated)> = Vec::new();
    for c in chunks {
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

pub(super) fn reduce_list(catalogs: &CatalogState, chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    let Some(cat) = catalogs.index() else {
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

/// Six counters per shard, summed. `drift` and `checked` are the two the verb
/// exists for: `checked` is how many held entries were re-read against the
/// keyspace, `drift` is how many disagreed. A write-hook-maintained index
/// should always report `drift 0` — VERIFY is what makes that falsifiable.
/// It used to report four counters and no drift at all, while the registry
/// and the docs advertised it.
pub(super) fn reduce_verify(chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut sums = [0u64; 6];
    for c in chunks {
        let mut pos = 1usize;
        for slot in &mut sums {
            let Some(w) = c.get(pos..pos + 8) else { break };
            *slot += u64::from_le_bytes(w.try_into().expect("8 bytes"));
            pos += 8;
        }
    }
    let labels: [&[u8]; 6] = [
        b"entries",
        b"bytes",
        b"coerce_failures",
        b"duplicates",
        b"drift",
        b"checked",
    ];
    encode_array_len(&mut out, 12);
    for (label, v) in labels.iter().zip(sums.iter()) {
        encode_bulk(&mut out, label);
        encode_bulk(&mut out, v.to_string().as_bytes());
    }
    out
}
