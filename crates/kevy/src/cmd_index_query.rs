//! v2.5 — IDX.* read surface: the extension fan-out halves
//! (per-shard op + origin reduce) and the query grammar. Split from
//! [`crate::cmd_index`] under the 500-LOC house rule.

use kevy_index::{Cursor, IndexValue, ValType};
use kevy_store::Store;

use crate::index_runtime;

/// One hit's hydrated field values (None = field absent).
pub(crate) type Hydrated = Vec<Option<Vec<u8>>>;

// ---------- extension fan-out (reads) ----------

pub(crate) const ST_OK: u8 = 0;
pub(crate) const ST_BUILDING: u8 = 1;
pub(crate) const ST_NOINDEX: u8 = 2;
pub(crate) const ST_BADARGS: u8 = 3;
pub(crate) const ST_OVERBUDGET: u8 = 4;

/// Per-shard half: parse the IDX.* argv, run against this shard's
/// segment, emit a status-tagged chunk.
pub(crate) fn extension_op(store: &mut Store, argv: &[Vec<u8>]) -> Vec<u8> {
    let verb = argv.first().map(Vec::as_slice).unwrap_or(b"");
    if verb.eq_ignore_ascii_case(b"IDX.LIST") {
        return op_list(store);
    }
    if argv.get(1).is_some_and(|a| a.eq_ignore_ascii_case(b"HYBRID")) {
        return op_hybrid(store, argv);
    }
    if argv.get(1).is_some_and(|a| a.eq_ignore_ascii_case(b"COMPOSE")) {
        return op_compose(store, argv);
    }
    // v3.10: IDX.EXPLAIN <name> <shape…> — the exact IDX.QUERY parse,
    // ZERO execution. Chunk: [ST_OK][building u8][entries u64 LE]
    // [shape byte] — kind/plan text assemble on the origin from the
    // catalog spec.
    if verb.eq_ignore_ascii_case(b"IDX.EXPLAIN") {
        return op_explain(store, argv);
    }
    // v2.7: IDX.QUERY <name> MATCH <text> [LIMIT n] [FIELDS f…]
    if argv.get(2).is_some_and(|a| a.eq_ignore_ascii_case(b"MATCH")) {
        return op_match(store, argv);
    }
    // v2.8: IDX.QUERY <name> KNN <vec> [LIMIT k] [FIELDS f…]
    if argv.get(2).is_some_and(|a| a.eq_ignore_ascii_case(b"KNN")) {
        return op_knn(store, argv);
    }
    // v3.1: IDX.QUERY <name> GROUP <g> | GROUPS [BY m] [LIMIT n]
    if argv.get(2).is_some_and(|a| a.eq_ignore_ascii_case(b"GROUP") || a.eq_ignore_ascii_case(b"GROUPS")) {
        return op_agg(store, argv);
    }
    // v3.1 phase 2 (internal): AGG.FETCH <name> <g…> — exact partials
    // for the candidate groups that survived phase-1 ranking.
    if argv.first().is_some_and(|v| v.eq_ignore_ascii_case(b"AGG.FETCH")) {
        let res = index_runtime::with_ready_agg(store, &argv[1], |a| {
            argv[2..].iter().map(|g| (g.clone(), a.group(g))).collect::<Vec<_>>()
        });
        return match res {
            Ok(rows) => encode_agg_chunk(&rows),
            Err(e) if e.starts_with("INDEXBUILDING") => vec![ST_BUILDING],
            Err(_) => vec![ST_NOINDEX],
        };
    }
    // v2.8: IDX.REBUILD <name> (ANN tombstone compaction)
    if argv
        .first()
        .is_some_and(|v| v.eq_ignore_ascii_case(b"IDX.REBUILD"))
    {
        let Some(name) = argv.get(1) else {
            return vec![ST_BADARGS];
        };
        return match index_runtime::with_ready_ann(store, name, |g| g.rebuild()) {
            Ok(()) => vec![ST_OK],
            Err(e) if e.starts_with("INDEXBUILDING") => vec![ST_BUILDING],
            Err(_) => vec![ST_NOINDEX],
        };
    }
    let Some(q) = Query::parse(argv) else {
        return vec![ST_BADARGS];
    };
    // Aggregate indexes answer VERIFY with their group stats.
    if matches!(q.shape, Shape::Verify)
        && index_runtime::catalog()
            .and_then(|c| c.get(&q.name).map(|(s, _)| s.kind))
            == Some(kevy_index::IndexKind::Agg)
    {
        return match index_runtime::with_ready_agg(store, &q.name, |a| a.stats()) {
            Ok(st) => {
                let mut chunk = vec![ST_OK];
                chunk.extend_from_slice(&st.rows.to_le_bytes());
                chunk.extend_from_slice(&st.approx_bytes.to_le_bytes());
                chunk.extend_from_slice(&st.excluded.to_le_bytes());
                chunk.extend_from_slice(&st.groups.to_le_bytes());
                chunk
            }
            Err(e) if e.starts_with("INDEXBUILDING") => vec![ST_BUILDING],
            Err(_) => vec![ST_NOINDEX],
        };
    }
    // ANN indexes answer VERIFY with their graph stats.
    if matches!(q.shape, Shape::Verify)
        && index_runtime::catalog()
            .and_then(|c| c.get(&q.name).map(|(s, _)| s.kind))
            == Some(kevy_index::IndexKind::Ann)
    {
        return match index_runtime::with_ready_ann(store, &q.name, |g| g.stats()) {
            Ok(st) => {
                let mut chunk = vec![ST_OK];
                chunk.extend_from_slice(&st.vectors.to_le_bytes());
                chunk.extend_from_slice(&st.approx_bytes.to_le_bytes());
                chunk.extend_from_slice(&st.tombstones.to_le_bytes());
                chunk.extend_from_slice(&(st.links + u64::from(st.rebuild_recommended)).to_le_bytes());
                chunk
            }
            Err(e) if e.starts_with("INDEXBUILDING") => vec![ST_BUILDING],
            Err(_) => vec![ST_NOINDEX],
        };
    }
    // Text indexes answer VERIFY with their text stats.
    if matches!(q.shape, Shape::Verify)
        && index_runtime::catalog()
            .and_then(|c| c.get(&q.name).map(|(s, _)| s.kind))
            == Some(kevy_index::IndexKind::Text)
    {
        return match index_runtime::with_ready_text_segment(store, &q.name, |ts| ts.stats()) {
            Ok(st) => {
                let mut chunk = vec![ST_OK];
                chunk.extend_from_slice(&st.docs.to_le_bytes());
                chunk.extend_from_slice(&st.approx_bytes.to_le_bytes());
                chunk.extend_from_slice(&st.postings.to_le_bytes());
                chunk.extend_from_slice(&st.tokens.to_le_bytes());
                chunk
            }
            Err(e) if e.starts_with("INDEXBUILDING") => vec![ST_BUILDING],
            Err(_) => vec![ST_NOINDEX],
        };
    }
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
        Err(e) if e.starts_with("INDEXOVERBUDGET") => vec![ST_OVERBUDGET],
        Err(_) => vec![ST_NOINDEX],
    }
}

/// v2.7 text MATCH per-shard: BM25-ranked hits + owning-shard
/// hydration. Chunk: `[ST_OK][n][(klen,key,score f64,fcount,fields)*]`.
fn op_match(store: &mut Store, argv: &[Vec<u8>]) -> Vec<u8> {
    let Some(q) = MatchArgs::parse(argv) else {
        return vec![ST_BADARGS];
    };
    let res = index_runtime::with_ready_text_segment(store, &q.name, |ts| {
        ts.matches(&q.text, q.limit)
    });
    match res {
        Ok(hits) => {
            let mut chunk = vec![ST_OK];
            chunk.extend_from_slice(&(hits.len() as u32).to_le_bytes());
            for h in &hits {
                chunk.extend_from_slice(&(h.key.len() as u32).to_le_bytes());
                chunk.extend_from_slice(&h.key);
                chunk.extend_from_slice(&h.score.to_le_bytes());
                encode_hydration(store, &mut chunk, &h.key, &q.fields);
            }
            chunk
        }
        Err(e) if e.starts_with("INDEXBUILDING") => vec![ST_BUILDING],
        Err(e) if e.starts_with("INDEXOVERBUDGET") => vec![ST_OVERBUDGET],
        Err(_) => vec![ST_NOINDEX],
    }
}

/// v3.1 agg per-shard. Chunk (both shapes):
/// `[ST_OK][n][(glen,group,count u64,sum f64,minflag+min,maxflag+max)*]`
/// — GROUP sends the one requested group; GROUPS sends every local
/// group (the reduce needs full partials to merge exactly).
fn op_agg(store: &mut Store, argv: &[Vec<u8>]) -> Vec<u8> {
    let single = argv[2].eq_ignore_ascii_case(b"GROUP");
    if single {
        let res = index_runtime::with_ready_agg(store, &argv[1], |a| {
            argv.get(3).map(|g| vec![(g.clone(), a.group(g))])
        });
        return match res {
            Ok(None) => vec![ST_BADARGS],
            Ok(Some(rows)) => encode_agg_chunk(&rows),
            Err(e) if e.starts_with("INDEXBUILDING") => vec![ST_BUILDING],
            Err(e) if e.starts_with("INDEXOVERBUDGET") => vec![ST_OVERBUDGET],
            Err(_) => vec![ST_NOINDEX],
        };
    }
    // GROUPS phase 1 (distributed exact top-K, TPUT-style): send only
    // the local top-(4×limit) by the ranking metric plus an
    // `exhausted` flag (a shard that sent EVERYTHING has τ = nothing
    // unsent — the reduce's uncertainty test needs to know). Full
    // materialization measured 14-18ms at 8×10k groups; this keeps
    // chunks at ~4·limit rows.
    let Some((by, limit)) = parse_groups_args(argv) else {
        return vec![ST_BADARGS];
    };
    let depth: usize = argv
        .iter()
        .find_map(|a| std::str::from_utf8(a).ok()?.strip_prefix("DEPTH=")?.parse().ok())
        .unwrap_or(1);
    let res = index_runtime::with_ready_agg(store, &argv[1], |a| {
        if depth == 0 {
            // fallback sentinel: full local materialization (uniform
            // near-tie data is unprunable — see reduce_agg)
            return (a.all_groups(), true);
        }
        let fetch = (limit * 4 * depth).max(64 * depth);
        let rows = a.top_groups(by, fetch);
        let exhausted = rows.len() < fetch;
        (rows, exhausted)
    });
    match res {
        Ok((rows, exhausted)) => {
            let mut chunk = encode_agg_chunk(&rows);
            chunk.push(u8::from(exhausted));
            chunk
        }
        Err(e) if e.starts_with("INDEXBUILDING") => vec![ST_BUILDING],
        Err(e) if e.starts_with("INDEXOVERBUDGET") => vec![ST_OVERBUDGET],
        Err(_) => vec![ST_NOINDEX],
    }
}

/// `GROUPS [BY m] [LIMIT n]` tail — shared by op and reduce.
pub(crate) fn parse_groups_args(argv: &[Vec<u8>]) -> Option<(kevy_index::AggBy, usize)> {
    let (mut by, mut limit) = (kevy_index::AggBy::Count, 100usize);
    let mut i = 3;
    while i < argv.len() {
        if argv[i].eq_ignore_ascii_case(b"BY") {
            by = kevy_index::AggBy::parse(argv.get(i + 1)?)?;
            i += 2;
        } else if argv[i].eq_ignore_ascii_case(b"LIMIT") {
            limit = std::str::from_utf8(argv.get(i + 1)?).ok()?.parse().ok()?;
            i += 2;
        } else if argv[i].starts_with(b"DEPTH=") {
            i += 1; // internal iterative-deepening marker
        } else {
            return None;
        }
    }
    Some((by, limit.clamp(1, 1000)))
}

/// Shared agg chunk encoding: `[ST_OK][n][(glen,g,count,sum,mmlen,mm)*]`.
fn encode_agg_chunk(rows: &[(Vec<u8>, kevy_index::GroupStats)]) -> Vec<u8> {
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

/// v2.8 KNN per-shard: distance-ranked hits + hydration. Chunk:
/// `[ST_OK][n][(klen,key,dist f64,fcount,fields)*]` — same layout as
/// MATCH chunks, so the reduce shares the decoder.
fn op_knn(store: &mut Store, argv: &[Vec<u8>]) -> Vec<u8> {
    let Some(q) = KnnArgs::parse(argv) else {
        return vec![ST_BADARGS];
    };
    let res = index_runtime::with_ready_ann(store, &q.name, |g| {
        kevy_vector::parse_vector(&q.vec, g.dim()).map(|v| g.knn(&v, q.limit, q.ef))
    });
    match res {
        Ok(None) => vec![ST_BADARGS], // vector doesn't match DIM
        Ok(Some(hits)) => {
            let mut chunk = vec![ST_OK];
            chunk.extend_from_slice(&(hits.len() as u32).to_le_bytes());
            for (key, dist) in &hits {
                chunk.extend_from_slice(&(key.len() as u32).to_le_bytes());
                chunk.extend_from_slice(key);
                chunk.extend_from_slice(&f64::from(*dist).to_le_bytes());
                encode_hydration(store, &mut chunk, key, &q.fields);
            }
            chunk
        }
        Err(e) if e.starts_with("INDEXBUILDING") => vec![ST_BUILDING],
        Err(e) if e.starts_with("INDEXOVERBUDGET") => vec![ST_OVERBUDGET],
        Err(_) => vec![ST_NOINDEX],
    }
}

/// `IDX.QUERY name KNN vec [LIMIT k] [FIELDS f…]` (no cursor; k ≤
/// 1000 — same rationale as MATCH).
pub(crate) struct KnnArgs {
    pub(crate) name: Vec<u8>,
    pub(crate) vec: Vec<u8>,
    pub(crate) limit: usize,
    /// Query beam width (`EF`); 0 = engine default.
    pub(crate) ef: usize,
    pub(crate) fields: Vec<Vec<u8>>,
}

impl KnnArgs {
    pub(crate) fn parse(argv: &[Vec<u8>]) -> Option<KnnArgs> {
        let name = argv.get(1)?.clone();
        if !argv.get(2)?.eq_ignore_ascii_case(b"KNN") {
            return None;
        }
        let vec = argv.get(3)?.clone();
        let mut limit = 10usize;
        let mut ef = 0usize;
        let mut fields = Vec::new();
        let mut i = 4;
        while i < argv.len() {
            let t = &argv[i];
            if t.eq_ignore_ascii_case(b"LIMIT") {
                limit = std::str::from_utf8(argv.get(i + 1)?).ok()?.parse().ok()?;
                i += 2;
            } else if t.eq_ignore_ascii_case(b"EF") {
                ef = std::str::from_utf8(argv.get(i + 1)?).ok()?.parse().ok()?;
                if !(16..=4096).contains(&ef) {
                    return None;
                }
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
        Some(KnnArgs { name, vec, limit: limit.clamp(1, 1000), ef, fields })
    }
}

/// `IDX.QUERY name MATCH text [LIMIT n] [FIELDS f…]` (no cursor —
/// BM25 deep pagination is an anti-pattern; LIMIT caps at 1000).
/// `IDX.QUERY HYBRID <text_idx> MATCH <q> <ann_idx> KNN <vec>
/// [LIMIT n] [RRFK k] [EF ef] [FIELDS f…]` — reciprocal-rank fusion
/// of a BM25 list and a KNN list over the same prefix (v3.13).
pub(crate) struct HybridArgs {
    pub(crate) text_idx: Vec<u8>,
    pub(crate) text: Vec<u8>,
    pub(crate) ann_idx: Vec<u8>,
    pub(crate) vec: Vec<u8>,
    pub(crate) limit: usize,
    pub(crate) rrf_k: f64,
    pub(crate) ef: usize,
    pub(crate) fields: Vec<Vec<u8>>,
}

impl HybridArgs {
    pub(crate) fn parse(argv: &[Vec<u8>]) -> Option<HybridArgs> {
        if !argv.get(1)?.eq_ignore_ascii_case(b"HYBRID")
            || !argv.get(3)?.eq_ignore_ascii_case(b"MATCH")
            || !argv.get(6)?.eq_ignore_ascii_case(b"KNN")
        {
            return None;
        }
        let mut a = HybridArgs {
            text_idx: argv.get(2)?.clone(),
            text: argv.get(4)?.clone(),
            ann_idx: argv.get(5)?.clone(),
            vec: argv.get(7)?.clone(),
            limit: 10,
            rrf_k: 60.0,
            ef: 0,
            fields: Vec::new(),
        };
        let mut i = 8;
        while i < argv.len() {
            let t = &argv[i];
            if t.eq_ignore_ascii_case(b"LIMIT") {
                a.limit = std::str::from_utf8(argv.get(i + 1)?).ok()?.parse().ok()?;
                if !(1..=1000).contains(&a.limit) {
                    return None;
                }
                i += 2;
            } else if t.eq_ignore_ascii_case(b"RRFK") {
                a.rrf_k = std::str::from_utf8(argv.get(i + 1)?).ok()?.parse().ok()?;
                if !a.rrf_k.is_finite() || a.rrf_k <= 0.0 {
                    return None;
                }
                i += 2;
            } else if t.eq_ignore_ascii_case(b"EF") {
                a.ef = std::str::from_utf8(argv.get(i + 1)?).ok()?.parse().ok()?;
                if !(16..=4096).contains(&a.ef) {
                    return None;
                }
                i += 2;
            } else if t.eq_ignore_ascii_case(b"FIELDS") {
                a.fields = argv[i + 1..].to_vec();
                if a.fields.is_empty() {
                    return None;
                }
                break;
            } else {
                return None;
            }
        }
        Some(a)
    }
}

/// Per-shard HYBRID: run BOTH sub-queries at 4×limit depth (rank
/// fusion needs deeper lists than the final cut — same 4× posture as
/// the agg TPUT phase-1), emit two ranked segments back to back.
/// Chunk: `[ST_OK][match n][(key,f64,hydration)*][knn n][(…)*]`.
fn op_hybrid(store: &mut Store, argv: &[Vec<u8>]) -> Vec<u8> {
    let Some(q) = HybridArgs::parse(argv) else {
        return vec![ST_BADARGS];
    };
    let depth = q.limit * 4;
    let m = index_runtime::with_ready_text_segment(store, &q.text_idx, |ts| {
        ts.matches(&q.text, depth)
    });
    let k = index_runtime::with_ready_ann(store, &q.ann_idx, |g| {
        kevy_vector::parse_vector(&q.vec, g.dim()).map(|v| g.knn(&v, depth, q.ef))
    });
    let (m, k) = match (m, k) {
        (Ok(m), Ok(Some(k))) => (m, k),
        (_, Ok(None)) => return vec![ST_BADARGS],
        (Err(e), _) | (_, Err(e)) if e.starts_with("INDEXBUILDING") => {
            return vec![ST_BUILDING];
        }
        (Err(e), _) | (_, Err(e)) if e.starts_with("INDEXOVERBUDGET") => {
            return vec![ST_OVERBUDGET];
        }
        _ => return vec![ST_NOINDEX],
    };
    let mut chunk = vec![ST_OK];
    chunk.extend_from_slice(&(m.len() as u32).to_le_bytes());
    for h in &m {
        chunk.extend_from_slice(&(h.key.len() as u32).to_le_bytes());
        chunk.extend_from_slice(&h.key);
        chunk.extend_from_slice(&h.score.to_le_bytes());
        encode_hydration(store, &mut chunk, &h.key, &q.fields);
    }
    chunk.extend_from_slice(&(k.len() as u32).to_le_bytes());
    for (key, dist) in &k {
        chunk.extend_from_slice(&(key.len() as u32).to_le_bytes());
        chunk.extend_from_slice(key);
        chunk.extend_from_slice(&f64::from(*dist).to_le_bytes());
        encode_hydration(store, &mut chunk, key, &q.fields);
    }
    chunk
}

pub(crate) struct MatchArgs {
    pub(crate) name: Vec<u8>,
    pub(crate) text: Vec<u8>,
    pub(crate) limit: usize,
    pub(crate) fields: Vec<Vec<u8>>,
}

impl MatchArgs {
    pub(crate) fn parse(argv: &[Vec<u8>]) -> Option<MatchArgs> {
        let name = argv.get(1)?.clone();
        if !argv.get(2)?.eq_ignore_ascii_case(b"MATCH") {
            return None;
        }
        let text = argv.get(3)?.clone();
        let mut limit = 10usize;
        let mut fields = Vec::new();
        let mut i = 4;
        while i < argv.len() {
            let t = &argv[i];
            if t.eq_ignore_ascii_case(b"LIMIT") {
                limit = std::str::from_utf8(argv.get(i + 1)?).ok()?.parse().ok()?;
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
        Some(MatchArgs { name, text, limit: limit.clamp(1, 1000), fields })
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
        Err(e) if e.starts_with("INDEXOVERBUDGET") => vec![ST_OVERBUDGET],
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

fn op_explain(store: &mut Store, argv: &[Vec<u8>]) -> Vec<u8> {
    let Some(cat) = index_runtime::catalog() else {
        return vec![ST_NOINDEX];
    };
    let name = argv.get(1).map(Vec::as_slice).unwrap_or(b"");
    let Some(spec) = cat.iter().map(|(s, _)| s).find(|s| s.name.as_slice() == name) else {
        return vec![ST_NOINDEX];
    };
    // Dry-run the same parses IDX.QUERY would run — arity/shape errors
    // surface here without touching the segment.
    let shape = argv.get(2).map(Vec::as_slice).unwrap_or(b"");
    let mut qargv = argv.to_vec();
    qargv[0] = b"IDX.QUERY".to_vec();
    let parsed = if name.eq_ignore_ascii_case(b"HYBRID") {
        // IDX.EXPLAIN HYBRID <text_idx> MATCH … — spec position 1 is
        // the mode, not an index; dry-run the hybrid parse.
        crate::cmd_index_query::HybridArgs::parse(&qargv).is_some()
    } else if shape.eq_ignore_ascii_case(b"MATCH") {
        crate::cmd_index_query::MatchArgs::parse(&qargv).is_some()
    } else if shape.eq_ignore_ascii_case(b"KNN") {
        KnnArgs::parse(&qargv).is_some()
    } else if shape.eq_ignore_ascii_case(b"GROUP") || shape.eq_ignore_ascii_case(b"GROUPS") {
        shape.eq_ignore_ascii_case(b"GROUP") || parse_groups_args(&qargv).is_some()
    } else {
        Query::parse(&qargv).is_some()
    };
    if !parsed {
        return vec![ST_BADARGS];
    }
    let building = index_runtime::segment_building(store, &spec.name);
    let entries = kind_entries(store, spec.kind, &spec.name);
    let mut chunk = vec![ST_OK, u8::from(building)];
    chunk.extend_from_slice(&entries.to_le_bytes());
    chunk.push(shape.first().copied().unwrap_or(b'?').to_ascii_uppercase());
    chunk
}

/// This shard's live row count for one index, by kind.
fn kind_entries(store: &mut Store, kind: kevy_index::IndexKind, name: &[u8]) -> u64 {
    match kind {
        kevy_index::IndexKind::Agg => {
            index_runtime::with_ready_agg(store, name, |a| a.stats().rows).unwrap_or_default()
        }
        kevy_index::IndexKind::Ann => {
            index_runtime::with_ready_ann(store, name, |g| g.stats().vectors).unwrap_or_default()
        }
        kevy_index::IndexKind::Text => {
            index_runtime::with_ready_text_segment(store, name, |t| t.stats().docs)
                .unwrap_or_default()
        }
        _ => index_runtime::with_ready_segment(store, name, |_, s| s.stats().entries)
            .unwrap_or_default(),
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
        // (entries, bytes, coerce_failures/postings, duplicates/tokens)
        let quad = if spec.kind == kevy_index::IndexKind::Agg {
            index_runtime::with_ready_agg(store, &spec.name, |a| {
                let st = a.stats();
                (st.rows, st.approx_bytes, st.excluded, st.groups)
            })
            .unwrap_or_default()
        } else if spec.kind == kevy_index::IndexKind::Ann {
            index_runtime::with_ready_ann(store, &spec.name, |g| {
                let st = g.stats();
                (st.vectors, st.approx_bytes, st.tombstones, st.links)
            })
            .unwrap_or_default()
        } else if spec.kind == kevy_index::IndexKind::Text {
            index_runtime::with_ready_text_segment(store, &spec.name, |ts| {
                let st = ts.stats();
                (st.docs, st.approx_bytes, st.postings, st.tokens)
            })
            .unwrap_or_default()
        } else {
            index_runtime::with_ready_segment(store, &spec.name, |_, seg| {
                let st = seg.stats();
                (st.entries, st.approx_bytes, st.coerce_failures, st.duplicates)
            })
            .unwrap_or_default()
        };
        chunk.push(u8::from(building));
        chunk.extend_from_slice(&quad.0.to_le_bytes());
        chunk.extend_from_slice(&quad.1.to_le_bytes());
        chunk.extend_from_slice(&quad.2.to_le_bytes());
        chunk.extend_from_slice(&quad.3.to_le_bytes());
    }
    chunk
}

pub(crate) fn decode_view_cursor(raw: &[u8]) -> Option<(IndexValue, Vec<u8>)> {
    decode_cursor(raw).map(|c| (c.value, c.key))
}

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

// ---------- query grammar ----------

pub(crate) enum Shape {
    Range { min: Vec<u8>, max: Vec<u8> },
    Eq { value: Vec<u8> },
    Verify,
}

/// One side of a COMPOSE (name + its shape).
pub(crate) struct SubQuery {
    name: Vec<u8>,
    shape: Shape,
}

pub(crate) struct Query {
    pub(crate) name: Vec<u8>,
    pub(crate) shape: Shape,
    pub(crate) limit: usize,
    pub(crate) cursor_raw: Option<Vec<u8>>,
    /// `FIELDS f…` hydration list (owning-shard hash reads ride the
    /// chunk; empty = keys/values only).
    pub(crate) fields: Vec<Vec<u8>>,
}

/// `IDX.QUERY COMPOSE AND|OR sub1 sub2 …` — key-ordered (the two
/// indexes' value domains differ, so composition orders by key and
/// the cursor is a plain key point).
pub(crate) struct ComposeQuery {
    pub(crate) and: bool,
    pub(crate) a: SubQuery,
    pub(crate) b: SubQuery,
    pub(crate) limit: usize,
    pub(crate) cursor_key: Option<Vec<u8>>,
    pub(crate) fields: Vec<Vec<u8>>,
}

impl ComposeQuery {
    /// `IDX.QUERY COMPOSE AND|OR nameA <shapeA> nameB <shapeB>
    /// [LIMIT n] [CURSOR k] [FIELDS f…]` where shape =
    /// `RANGE min max` | `EQ v`.
    pub(crate) fn parse(argv: &[Vec<u8>]) -> Option<ComposeQuery> {
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

pub(crate) fn hex(b: &[u8]) -> Vec<u8> {
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
    pub(crate) fn parse(argv: &[Vec<u8>]) -> Option<Query> {
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
