//! v2.5 — IDX.* origin-side reduce: merge per-shard chunks into RESP
//! (split from [`crate::cmd_index_query`] under the 500-LOC rule).

use kevy_index::IndexValue;
use kevy_resp::{encode_array_len, encode_bulk, encode_error, encode_integer};

use crate::cmd_index_query::{
    ComposeQuery, Hydrated, Query, ST_BADARGS, ST_BUILDING, ST_NOINDEX, ST_OVERBUDGET,
    decode_value, encode_value, hex,
};
use crate::index_runtime;

/// Origin half: merge chunks → RESP.
pub(crate) fn extension_reduce(argv: &[Vec<u8>], chunks: Vec<Vec<u8>>) -> Vec<u8> {
    let verb = argv.first().map(Vec::as_slice).unwrap_or(b"");
    let mut out = Vec::new();
    // Status triage: any BADARGS / NOINDEX / BUILDING wins the reply.
    // v3.10: errors are SELF-EXPLAINING — they name the verb and the
    // index and point at the discovery surface, so an agent that hits
    // one can recover without out-of-band knowledge.
    let verb_s = String::from_utf8_lossy(verb);
    let name_s = argv.get(1).map(|a| String::from_utf8_lossy(a).into_owned()).unwrap_or_default();
    for c in &chunks {
        match c.first().copied() {
            Some(ST_BADARGS) | None => {
                encode_error(
                    &mut out,
                    &format!("ERR {verb_s} '{name_s}': bad arguments — run COMMAND DOCS {verb_s} for the syntax"),
                );
                return out;
            }
            Some(ST_NOINDEX) => {
                encode_error(
                    &mut out,
                    &format!("ERR no such index '{name_s}' (IDX.LIST enumerates them)"),
                );
                return out;
            }
            Some(ST_BUILDING) => {
                encode_error(
                    &mut out,
                    &format!("INDEXBUILDING index '{name_s}' is still building (poll IDX.LIST until state=ready)"),
                );
                return out;
            }
            Some(ST_OVERBUDGET) => {
                encode_error(
                    &mut out,
                    &format!("INDEXOVERBUDGET index '{name_s}' build exceeded MAXMEM (raise maxmemory or DROP the index)"),
                );
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
    // v3.1 GROUP/GROUPS: TPUT-style exact top-K (see reduce_agg).
    if argv.first().is_some_and(|v| v.eq_ignore_ascii_case(b"AGG.FETCH")) {
        return reduce_agg_fetch(argv, &chunks);
    }
    if argv.get(2).is_some_and(|a| a.eq_ignore_ascii_case(b"GROUP") || a.eq_ignore_ascii_case(b"GROUPS")) {
        return reduce_agg(argv, &chunks);
    }
    // v2.8 KNN: merge distance-ranked chunks ascending (smaller =
    // closer for every metric).
    if argv.get(2).is_some_and(|a| a.eq_ignore_ascii_case(b"KNN")) {
        return reduce_ranked(argv, &chunks, true);
    }

    // v2.8 REBUILD: all shards OK → +OK.
    if argv
        .first()
        .is_some_and(|v| v.eq_ignore_ascii_case(b"IDX.REBUILD"))
    {
        for c in &chunks {
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
        return out;
    }
    // v2.7 MATCH / v2.8 KNN: merge ranked chunks (identical layout);
    // MATCH sorts score-descending, KNN distance-ascending.
    if argv.get(2).is_some_and(|a| a.eq_ignore_ascii_case(b"MATCH")) {
        return reduce_ranked(argv, &chunks, false);
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

/// v2.6: view reduce reuses the (value,key) cursor encoding.
pub(crate) fn encode_view_cursor_bytes(v: &IndexValue, k: &[u8]) -> Vec<u8> {
    encode_cursor(v, k)
}

/// v2.6: shared chunk readers + value repr for the view reduce.
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

fn encode_cursor(v: &IndexValue, k: &[u8]) -> Vec<u8> {
    let mut payload = Vec::new();
    encode_value(&mut payload, v);
    payload.extend_from_slice(k);
    hex(&payload)
}

/// Shared MATCH/KNN reduce: decode `[n][(key, f64, hydration)*]`
/// chunks, sort (ascending for KNN distances, descending for BM25
/// scores), truncate to LIMIT, emit `[key, value, fields…]` rows.
fn reduce_ranked(argv: &[Vec<u8>], chunks: &[Vec<u8>], ascending: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let (limit, fields) = if ascending {
        match crate::cmd_index_query::KnnArgs::parse(argv) {
            Some(q) => (q.limit, q.fields),
            None => {
                encode_error(&mut out, "ERR bad IDX arguments");
                return out;
            }
        }
    } else {
        match crate::cmd_index_query::MatchArgs::parse(argv) {
            Some(q) => (q.limit, q.fields),
            None => {
                encode_error(&mut out, "ERR bad IDX arguments");
                return out;
            }
        }
    };
    let mut all: Vec<(f64, Vec<u8>, Hydrated)> = Vec::new();
    for c in chunks {
        let mut pos = 1usize;
        let Some(n) = read_u32(c, &mut pos) else { continue };
        for _ in 0..n {
            let Some(key) = read_kbytes(c, &mut pos) else { break };
            let Some(sb) = c.get(pos..pos + 8) else { break };
            let v = f64::from_le_bytes(sb.try_into().expect("8 bytes"));
            pos += 8;
            let Some(fv) = read_hydration(c, &mut pos) else { break };
            all.push((v, key, fv));
        }
    }
    if ascending {
        all.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    } else {
        all.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    }
    all.truncate(limit);
    encode_array_len(&mut out, all.len() as i64);
    for (v, key, fv) in &all {
        let base = 2 + fields.len() * 2;
        encode_array_len(&mut out, base as i64);
        encode_bulk(&mut out, key);
        encode_bulk(&mut out, format!("{v:.4}").as_bytes());
        for (f, val) in fields.iter().zip(fv.iter().chain(std::iter::repeat(&None))) {
            encode_bulk(&mut out, f);
            match val {
                Some(b) => encode_bulk(&mut out, b),
                None => out.extend_from_slice(b"$-1\r\n"),
            }
        }
    }
    out
}

/// v3.1 reduce — distributed EXACT top-K over group aggregates
/// (TPUT-style). Phase 1 chunks carry each shard's local top-(4·limit)
/// by the ranking metric plus an `exhausted` flag; if the unseen mass
/// (Σ τ over non-exhausted shards) could displace the k-th candidate,
/// a continuation re-runs phase 1 with 4× depth (terminates: depth
/// reaches every group). Otherwise a targeted AGG.FETCH continuation
/// collects the survivors' exact partials from every shard.
///
/// History (agggate): full-materialization chunks 14-18ms at 8×10k
/// groups; this path answers from ~4·limit-row chunks.
fn reduce_agg(argv: &[Vec<u8>], chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    let single = argv[2].eq_ignore_ascii_case(b"GROUP");
    if single {
        // one group: chunks are exact partials already — merge & emit
        let mut st = kevy_index::GroupStats { count: 0, sum: 0.0, min: None, max: None };
        for c in chunks {
            for (_g, part) in decode_agg_chunk(c) {
                kevy_index::merge_group(&mut st, &part);
            }
        }
        encode_array_len(&mut out, 5);
        encode_bulk(&mut out, st.count.to_string().as_bytes());
        encode_bulk(&mut out, format!("{}", st.sum).as_bytes());
        for v in [&st.min, &st.max] {
            match v {
                Some(x) => encode_bulk(&mut out, &value_repr(x)),
                None => out.extend_from_slice(b"$-1\r\n"),
            }
        }
        match st.avg() {
            Some(a) => encode_bulk(&mut out, format!("{a}").as_bytes()),
            None => out.extend_from_slice(b"$-1\r\n"),
        }
        return out;
    }
    let Some((by, limit)) = crate::cmd_index_query::parse_groups_args(argv) else {
        encode_error(&mut out, "ERR bad IDX arguments");
        return out;
    };
    // ---- phase 1: merge observed partials + collect per-shard τ ----
    let additive = matches!(by, kevy_index::AggBy::Count | kevy_index::AggBy::Sum);
    let mut observed: std::collections::HashMap<Vec<u8>, kevy_index::GroupStats> =
        std::collections::HashMap::new();
    let mut taus: Vec<f64> = Vec::new(); // score bound per NON-exhausted shard
    for c in chunks {
        let rows = decode_agg_chunk(&c[..c.len().saturating_sub(1)]);
        let exhausted = c.last() == Some(&1);
        if !exhausted {
            let tau = rows.last().map_or(f64::NEG_INFINITY, |(_, st)| score(st, by));
            taus.push(tau);
        }
        for (g, part) in rows {
            match observed.get_mut(&g) {
                Some(st) => kevy_index::merge_group(st, &part),
                None => {
                    observed.insert(g, part);
                }
            }
        }
    }
    // rank observed by score; θ = k-th best observed score
    let mut ranked: Vec<(Vec<u8>, kevy_index::GroupStats)> = observed.into_iter().collect();
    ranked.sort_by(|a, b| score(&b.1, by).total_cmp(&score(&a.1, by)).then_with(|| a.0.cmp(&b.0)));
    let theta = ranked
        .get(limit - 1)
        .map_or(f64::NEG_INFINITY, |(_, st)| score(st, by));
    // uncertainty: could anything UNSEEN (or unseen mass of a seen
    // group) displace the k-th? Additive: bound = Σ τ; max-type:
    // bound = max τ.
    let unseen_bound = if additive {
        taus.iter().sum::<f64>()
    } else {
        taus.iter().copied().fold(f64::NEG_INFINITY, f64::max)
    };
    let depth = groups_depth(argv);
    if unseen_bound > theta && depth != 0 {
        // One 4× deepening, then jump STRAIGHT to full
        // materialization (DEPTH=0): uniform near-tie data defeats
        // thresholds information-theoretically, and a geometric crawl
        // just multiplies fan-out rounds (measured 51ms).
        let mut argv2: Vec<Vec<u8>> = argv.to_vec();
        set_groups_depth(&mut argv2, if depth == 1 { 4 } else { 0 });
        return continuation(&argv2);
    }
    // ---- phase 2: exact totals for potential top-k members ----
    // candidates = observed groups whose upper bound reaches θ
    let mut cands: Vec<Vec<u8>> = ranked
        .iter()
        .filter(|(_, st)| {
            let upper = if additive {
                score(st, by) + taus.iter().sum::<f64>()
            } else {
                score(st, by).max(unseen_bound)
            };
            upper >= theta
        })
        .map(|(g, _)| g.clone())
        .collect();
    cands.truncate((limit * 32).max(256)); // hard cap on fetch width
    let mut argv2: Vec<Vec<u8>> = vec![
        b"AGG.FETCH".to_vec(),
        argv[1].clone(),
        format!("BY={} LIMIT={}", tag_of(by), limit).into_bytes(),
    ];
    argv2.extend(cands);
    continuation(&argv2)
}

/// Final reduce for the AGG.FETCH phase: exact merge of the fetched
/// candidates, rank, truncate, emit.
fn reduce_agg_fetch(argv: &[Vec<u8>], chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    let meta = argv.get(2).map(|m| String::from_utf8_lossy(m).into_owned()).unwrap_or_default();
    let by = meta
        .split_whitespace()
        .find_map(|t| t.strip_prefix("BY=").and_then(|b| kevy_index::AggBy::parse(b.as_bytes())))
        .unwrap_or_default();
    let limit: usize = meta
        .split_whitespace()
        .find_map(|t| t.strip_prefix("LIMIT=").and_then(|n| n.parse().ok()))
        .unwrap_or(100);
    let mut merged: std::collections::HashMap<Vec<u8>, kevy_index::GroupStats> =
        std::collections::HashMap::new();
    for c in chunks {
        for (g, part) in decode_agg_chunk(c) {
            match merged.get_mut(&g) {
                Some(st) => kevy_index::merge_group(st, &part),
                None => {
                    merged.insert(g, part);
                }
            }
        }
    }
    let mut ranked: Vec<(Vec<u8>, kevy_index::GroupStats)> = merged
        .into_iter()
        .filter(|(_, st)| st.count > 0)
        .collect();
    kevy_index::sort_groups(&mut ranked, by);
    ranked.truncate(limit);
    encode_array_len(&mut out, ranked.len() as i64);
    for (g, st) in &ranked {
        encode_array_len(&mut out, 5);
        encode_bulk(&mut out, g);
        encode_bulk(&mut out, st.count.to_string().as_bytes());
        encode_bulk(&mut out, format!("{}", st.sum).as_bytes());
        for v in [&st.min, &st.max] {
            match v {
                Some(x) => encode_bulk(&mut out, &value_repr(x)),
                None => out.extend_from_slice(b"$-1\r\n"),
            }
        }
    }
    out
}

/// Ranking score, oriented bigger-is-better for every metric.
fn score(st: &kevy_index::GroupStats, by: kevy_index::AggBy) -> f64 {
    match by {
        kevy_index::AggBy::Count => st.count as f64,
        kevy_index::AggBy::Sum => st.sum,
        kevy_index::AggBy::Max => st.max.as_ref().map_or(f64::NEG_INFINITY, |v| v.as_f64()),
        kevy_index::AggBy::Min => st.min.as_ref().map_or(f64::NEG_INFINITY, |v| -v.as_f64()),
    }
}

fn tag_of(by: kevy_index::AggBy) -> &'static str {
    match by {
        kevy_index::AggBy::Count => "count",
        kevy_index::AggBy::Sum => "sum",
        kevy_index::AggBy::Min => "min",
        kevy_index::AggBy::Max => "max",
    }
}

/// Internal DEPTH arg (iterative deepening), default 1.
fn groups_depth(argv: &[Vec<u8>]) -> usize {
    argv.iter()
        .find_map(|a| {
            std::str::from_utf8(a).ok()?.strip_prefix("DEPTH=")?.parse().ok()
        })
        .unwrap_or(1)
}

fn set_groups_depth(argv: &mut Vec<Vec<u8>>, depth: usize) {
    for a in argv.iter_mut() {
        if a.starts_with(b"DEPTH=") {
            *a = format!("DEPTH={depth}").into_bytes();
            return;
        }
    }
    argv.push(format!("DEPTH={depth}").into_bytes());
}

/// Encode a stateless two-phase continuation (kevy-rt convention: a
/// reduce reply starting with NUL re-fans the embedded argv).
fn continuation(argv: &[Vec<u8>]) -> Vec<u8> {
    let mut cont = vec![0u8];
    cont.extend_from_slice(&(argv.len() as u32).to_le_bytes());
    for item in argv {
        cont.extend_from_slice(&(item.len() as u32).to_le_bytes());
        cont.extend_from_slice(item);
    }
    cont
}

/// Decode `[ST_OK][n][(glen,g,count,sum,mmlen,mm)*]`.
fn decode_agg_chunk(c: &[u8]) -> Vec<(Vec<u8>, kevy_index::GroupStats)> {
    let mut rows = Vec::new();
    let mut pos = 1usize;
    let Some(n) = read_u32(c, &mut pos) else { return rows };
    for _ in 0..n {
        let Some(g) = read_kbytes(c, &mut pos) else { break };
        let Some(cb) = c.get(pos..pos + 8) else { break };
        let count = u64::from_le_bytes(cb.try_into().expect("8"));
        pos += 8;
        let Some(sb) = c.get(pos..pos + 8) else { break };
        let sum = f64::from_le_bytes(sb.try_into().expect("8"));
        pos += 8;
        let Some(ml) = c.get(pos..pos + 4) else { break };
        let ml = u32::from_le_bytes(ml.try_into().expect("4")) as usize;
        pos += 4;
        let Some(mm) = c.get(pos..pos + ml) else { break };
        pos += ml;
        let mut mpos = 0usize;
        let mut vals = [None, None];
        for slot in &mut vals {
            match mm.get(mpos).copied() {
                Some(1) => {
                    mpos += 1;
                    *slot = crate::cmd_index_query::decode_value(mm, &mut mpos);
                }
                _ => mpos += 1,
            }
        }
        rows.push((g, kevy_index::GroupStats { count, sum, min: vals[0].clone(), max: vals[1].clone() }));
    }
    rows
}
