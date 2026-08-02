//! Scalar IDX.QUERY / IDX.COUNT / IDX.VERIFY per-shard execution plus
//! the IDX.EXPLAIN / IDX.LIST admin surface.

use kevy_index::{IndexSpec, IndexValue, SegmentStats};
use kevy_store::Store;

use super::args::{KnnArgs, Query, Shape, parse_groups_args};
use super::wire::{encode_hydration_row, encode_value, peek_hydration};
use super::{ST_BADARGS, ST_BUILDING, ST_NOINDEX, ST_OK, ST_OVERBUDGET};
use crate::index_runtime;
use crate::state::Ctx;

enum HitsOrChunk {
    Hits(Vec<(Vec<u8>, IndexValue)>),
    Chunk(Vec<u8>),
    /// A VERIFY snapshot: the segment's held entries plus its stats. The drift
    /// recheck needs the store, which the segment borrow holds, so it runs
    /// after that borrow ends — same shape as `encode_hits_chunk`'s hydration.
    Verify {
        spec: Box<IndexSpec>,
        entries: Vec<(Vec<u8>, IndexValue)>,
        stats: SegmentStats,
    },
}

/// The scalar tail of the IDX.* fan-out: parse the query grammar,
/// answer kind-specific VERIFY stats, else run the range/eq/verify
/// shape against this shard's segment.
pub(super) fn op_query(ctx: &Ctx<'_>, store: &mut Store, argv: &[Vec<u8>], verb: &[u8]) -> Vec<u8> {
    let Some(q) = Query::parse(argv) else {
        return vec![ST_BADARGS];
    };
    // IDX.COUNT applies FILTER (the claused count — the total a
    // claused query's pages would reach, materializing nothing) and
    // refuses every clause it would not apply: SORT/DISTINCT/OFFSET
    // cannot change a count, FACET/FIELDS have nowhere to go, and
    // accepting-then-ignoring any of them would be worse than the
    // refusal.
    if verb.eq_ignore_ascii_case(b"IDX.COUNT") {
        if q.selects() || !q.fields.is_empty() || q.cursor_raw.is_some() {
            return vec![ST_BADARGS];
        }
        if !q.filters.is_empty() {
            return super::query_claused::run_claused_count(ctx, store, &q);
        }
    }
    // Pure grammar, refused before the segment is consulted: the
    // selection clauses re-shape the page, so a resume point in the
    // driving order has nothing to resume.
    if q.cursor_raw.is_some() && q.selects() {
        return super::query_claused::clause_chunk(
            super::query_claused::CURSOR_CLAUSE_CONFLICT,
        );
    }
    if matches!(q.shape, Shape::Verify)
        && let Some(chunk) = verify_kind_stats(ctx, store, &q.name)
    {
        return chunk;
    }
    if q.has_clauses() {
        return super::query_claused::run_claused_query(ctx, store, &q);
    }
    run_scalar_query(ctx, store, &q, verb)
}

/// Aggregate / ANN / text indexes answer VERIFY with their own stats
/// (`None` = scalar kind, fall through to the segment path).
fn verify_kind_stats(ctx: &Ctx<'_>, store: &mut Store, name: &[u8]) -> Option<Vec<u8>> {
    let kind = ctx.state.catalogs.index().and_then(|c| c.get(name).map(|(s, _)| s.kind))?;
    let res = match kind {
        kevy_index::IndexKind::Agg => index_runtime::with_ready_agg(ctx, store, name, |a| {
            let st = a.stats();
            [st.rows, st.approx_bytes, st.excluded, st.groups]
        }),
        kevy_index::IndexKind::Ann => index_runtime::with_ready_ann(ctx, store, name, |g| {
            let st = g.stats();
            [
                st.vectors,
                st.approx_bytes,
                st.tombstones,
                st.links + u64::from(st.rebuild_recommended),
            ]
        }),
        kevy_index::IndexKind::Text => {
            index_runtime::with_ready_text_segment(ctx, store, name, |ts, _, _| {
                let st = ts.stats();
                [st.docs, st.approx_bytes, st.postings, st.tokens]
            })
        }
        _ => return None,
    };
    Some(match res {
        Ok(quad) => {
            let mut chunk = vec![ST_OK];
            for v in quad {
                chunk.extend_from_slice(&v.to_le_bytes());
            }
            chunk
        }
        Err(e) if e.as_wire().starts_with("INDEXBUILDING") => vec![ST_BUILDING],
        Err(_) => vec![ST_NOINDEX],
    })
}

/// Range / Eq / scalar-Verify against this shard's segment.
fn run_scalar_query(ctx: &Ctx<'_>, store: &mut Store, q: &Query, verb: &[u8]) -> Vec<u8> {
    let res = index_runtime::with_ready_segment(ctx, store, &q.name, |spec, seg, win| match q
        .shape
    {
        Shape::Range { .. } | Shape::Eq { .. } | Shape::Where(_) => {
            let (min, max) = match q.bounds_for(spec) {
                Ok(b) => b,
                Err(chunk) => return HitsOrChunk::Chunk(chunk),
            };
            scalar_range_or_count(q, verb, spec, seg, win, &min, &max)
        }
        // VERIFY answers "does the index still agree with the keyspace?".
        // The segment cannot be walked and the store re-read at the same time
        // (`with_ready_segment` holds the store), so snapshot the held
        // (key, value) pairs here and do the recheck outside — which is what
        // this arm was always shaped for, except the snapshot was collected,
        // thrown away with `let _ = (...)`, and the drift it was for never
        // computed. That left an O(N) walk plus an O(N) allocation per shard
        // per VERIFY, producing nothing, while `verb_meta` and the docs
        // advertised a drift statistic the reply did not carry.
        Shape::Verify => {
            let mut entries: Vec<(Vec<u8>, IndexValue)> = Vec::new();
            seg.each_entry(|k, v| entries.push((k.to_vec(), v.clone())));
            HitsOrChunk::Verify { spec: Box::new(spec.clone()), entries, stats: seg.stats() }
        }
    });
    match res {
        Ok(HitsOrChunk::Chunk(chunk)) => chunk,
        Ok(HitsOrChunk::Hits(hits)) => encode_hits_chunk(store, &hits, &q.fields),
        Ok(HitsOrChunk::Verify { spec, entries, stats }) => {
            encode_verify_chunk(store, &spec, &entries, &stats)
        }
        Err(e) if e.as_wire().starts_with("INDEXBUILDING") => vec![ST_BUILDING],
        Err(e) if e.as_wire().starts_with("INDEXOVERBUDGET") => vec![ST_OVERBUDGET],
        Err(_) => vec![ST_NOINDEX],
    }
}

/// The range/count body: hot tree plus — on a windowed index that has
/// evicted — the cold segments' half. A corrupt cold segment answers
/// ST_NOINDEX, never a partial number.
fn scalar_range_or_count(
    q: &Query,
    verb: &[u8],
    spec: &kevy_index::IndexSpec,
    seg: &kevy_index::Segment,
    win: Option<&index_runtime::WindowRt>,
    min: &IndexValue,
    max: &IndexValue,
) -> HitsOrChunk {
    let cold = win.filter(|w| w.has_cold());
    if verb.eq_ignore_ascii_case(b"IDX.COUNT") {
        let cold_n = match cold.map(|w| w.cold_count(spec.ty, min, max)).transpose() {
            Ok(n) => n.unwrap_or(0),
            Err(_) => return HitsOrChunk::Chunk(vec![ST_NOINDEX]),
        };
        let mut chunk = vec![ST_OK];
        chunk.extend_from_slice(&(seg.count(min, max) + cold_n).to_le_bytes());
        return HitsOrChunk::Chunk(chunk);
    }
    let cursor = q.cursor(spec.ty);
    let (hits, _) = seg.range(min, max, cursor.as_ref(), q.limit);
    match cold.map(|w| w.cold_hits(spec.ty, min, max, q.limit)).transpose() {
        Ok(None) => HitsOrChunk::Hits(hits),
        Ok(Some(cold_hits)) => HitsOrChunk::Hits(merge_cold(hits, cold_hits, &cursor, q.limit)),
        Err(_) => HitsOrChunk::Chunk(vec![ST_NOINDEX]),
    }
}

/// Merge the hot page with the cold hits: both ascend by
/// `(value, key)`, the cold side additionally drops everything at or
/// before the cursor (the hot side already did), and the page truncates
/// to `limit`.
fn merge_cold(
    hot: Vec<(Vec<u8>, IndexValue)>,
    cold: Vec<(Vec<u8>, IndexValue)>,
    cursor: &Option<kevy_index::Cursor>,
    limit: usize,
) -> Vec<(Vec<u8>, IndexValue)> {
    let after_cursor = |k: &[u8], v: &IndexValue| match cursor {
        None => true,
        Some(c) => (v, k) > (&c.value, c.key.as_slice()),
    };
    let mut out = Vec::with_capacity(hot.len() + cold.len());
    let (mut h, mut c) = (hot.into_iter().peekable(), cold.into_iter().peekable());
    while out.len() < limit {
        let take_cold = match (h.peek(), c.peek()) {
            (None, None) => break,
            (Some(_), None) => false,
            (None, Some(_)) => true,
            (Some((hk, hv)), Some((ck, cv))) => (cv, ck) < (hv, hk),
        };
        let (k, v) = if take_cold { c.next() } else { h.next() }.expect("peeked");
        if take_cold && !after_cursor(&k, &v) {
            continue;
        }
        out.push((k, v));
    }
    out
}

/// Hydration happens OUTSIDE the segment borrow: the hits' rows live
/// on this shard, plain hash reads.
fn encode_hits_chunk(
    store: &mut Store,
    hits: &[(Vec<u8>, IndexValue)],
    fields: &[Vec<u8>],
) -> Vec<u8> {
    let mut chunk = vec![ST_OK];
    chunk.extend_from_slice(&(hits.len() as u32).to_le_bytes());
    // Hydration rows prefetched as ONE batched page (cold rows
    // coalesce into one submission), then encoded in hit order.
    let keys: Vec<&[u8]> = hits.iter().map(|(k, _)| k.as_slice()).collect();
    let rows = peek_hydration(store, &keys, fields);
    for (i, (k, v)) in hits.iter().enumerate() {
        chunk.extend_from_slice(&(k.len() as u32).to_le_bytes());
        chunk.extend_from_slice(k);
        encode_value(&mut chunk, v);
        encode_hydration_row(&mut chunk, fields.len(), &rows[i]);
    }
    chunk
}

/// IDX.EXPLAIN <name> <shape…> — the exact IDX.QUERY parse,
/// ZERO execution. Chunk: [ST_OK][building u8][entries u64 LE]
/// [shape byte] — kind/plan text assemble on the origin from the
/// catalog spec.
pub(super) fn op_explain(ctx: &Ctx<'_>, store: &mut Store, argv: &[Vec<u8>]) -> Vec<u8> {
    let Some(cat) = ctx.state.catalogs.index() else {
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
        super::args::HybridArgs::parse(&qargv).is_some()
    } else if shape.eq_ignore_ascii_case(b"MATCH") {
        super::args::MatchArgs::parse(&qargv).is_some()
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
    let building = index_runtime::segment_building(ctx, store, &spec.name);
    let entries = kind_entries(ctx, store, spec.kind, &spec.name);
    let mut chunk = vec![ST_OK, u8::from(building)];
    chunk.extend_from_slice(&entries.to_le_bytes());
    chunk.push(shape.first().copied().unwrap_or(b'?').to_ascii_uppercase());
    chunk
}

/// This shard's live row count for one index, by kind.
fn kind_entries(ctx: &Ctx<'_>, store: &mut Store, kind: kevy_index::IndexKind, name: &[u8]) -> u64 {
    match kind {
        kevy_index::IndexKind::Agg => {
            index_runtime::with_ready_agg(ctx, store, name, |a| a.rows()).unwrap_or_default()
        }
        kevy_index::IndexKind::Ann => {
            index_runtime::with_ready_ann(ctx, store, name, |g| g.vectors()).unwrap_or_default()
        }
        kevy_index::IndexKind::Text => {
            index_runtime::with_ready_text_segment(ctx, store, name, |t, _, _| t.docs())
                .unwrap_or_default()
        }
        _ => index_runtime::with_ready_segment(ctx, store, name, |_, s, _| s.stats().entries)
            .unwrap_or_default(),
    }
}

pub(super) fn op_list(ctx: &Ctx<'_>, store: &mut Store) -> Vec<u8> {
    // Chunk: per declared index, this shard's (entries, bytes,
    // coerce_failures, duplicates, building-flag).
    let Some(cat) = ctx.state.catalogs.index() else {
        return vec![ST_OK];
    };
    let mut chunk = vec![ST_OK];
    for (spec, _) in cat.iter() {
        let building = index_runtime::segment_building(ctx, store, &spec.name);
        // (entries, bytes, coerce_failures/postings, duplicates/tokens)
        let quad = if spec.kind == kevy_index::IndexKind::Agg {
            index_runtime::with_ready_agg(ctx, store, &spec.name, |a| {
                let st = a.stats();
                (st.rows, st.approx_bytes, st.excluded, st.groups)
            })
            .unwrap_or_default()
        } else if spec.kind == kevy_index::IndexKind::Ann {
            index_runtime::with_ready_ann(ctx, store, &spec.name, |g| {
                let st = g.stats();
                (st.vectors, st.approx_bytes, st.tombstones, st.links)
            })
            .unwrap_or_default()
        } else if spec.kind == kevy_index::IndexKind::Text {
            index_runtime::with_ready_text_segment(ctx, store, &spec.name, |ts, _, _| {
                let st = ts.stats();
                (st.docs, st.approx_bytes, st.postings, st.tokens)
            })
            .unwrap_or_default()
        } else {
            index_runtime::with_ready_segment(ctx, store, &spec.name, |_, seg, _| {
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

/// The drift recheck, outside the segment borrow.
///
/// For every key the index holds, re-read its row from the keyspace and
/// re-coerce it exactly as the builder does. Three ways an entry can be
/// wrong, and all three count as drift:
///
///   * the row is gone but the index still holds the key,
///   * the row no longer coerces (the field was overwritten with a value the
///     index's type cannot take),
///   * the row coerces to a DIFFERENT value than the one indexed.
///
/// A write-hook-maintained index should never drift. `IDX.VERIFY` exists so
/// that claim is falsifiable instead of merely asserted — which is why the
/// number has to actually be computed.
fn encode_verify_chunk(
    store: &mut Store,
    spec: &IndexSpec,
    entries: &[(Vec<u8>, IndexValue)],
    stats: &SegmentStats,
) -> Vec<u8> {
    // VERIFY's recheck is a bulk sweep — inside the peek scope a
    // cold row costs one pread and never promotes or marks the gate.
    let drift = store.peek_scope(|s| {
        let mut drift = 0u64;
        for (key, held) in entries {
            match index_runtime::row_value(s, spec, key) {
                index_runtime::RowValue::Value(actual) if &actual == held => {}
                _ => drift += 1,
            }
        }
        drift
    });
    let mut chunk = vec![ST_OK];
    chunk.extend_from_slice(&stats.entries.to_le_bytes());
    chunk.extend_from_slice(&stats.approx_bytes.to_le_bytes());
    chunk.extend_from_slice(&stats.coerce_failures.to_le_bytes());
    chunk.extend_from_slice(&stats.duplicates.to_le_bytes());
    chunk.extend_from_slice(&drift.to_le_bytes());
    chunk.extend_from_slice(&(entries.len() as u64).to_le_bytes());
    chunk
}

#[cfg(test)]
mod verify_tests {
    use super::*;
    use kevy_index::{IndexKind, ValType};

    fn spec() -> IndexSpec {
        IndexSpec {
            name: b"byage".to_vec(),
            prefix: b"u:".to_vec(),
            fields: vec![kevy_index::FieldSpec::new(b"age".to_vec())],
            ty: ValType::I64,
            kind: IndexKind::Range,
            max_bytes: 0,
            ann: None,
            group_by: None,
            with_positions: false,
            values: Vec::new(),
            composite: None,
        }
    }

    fn stats() -> SegmentStats {
        SegmentStats { entries: 3, approx_bytes: 0, coerce_failures: 0, duplicates: 0 }
    }

    /// Read `drift` and `checked` back out of the wire chunk.
    fn drift_and_checked(chunk: &[u8]) -> (u64, u64) {
        let at = |i: usize| {
            u64::from_le_bytes(chunk[1 + i * 8..1 + (i + 1) * 8].try_into().expect("8 bytes"))
        };
        (at(4), at(5))
    }

    /// A drift counter that can only ever report zero is the dead code it
    /// replaced. Diverge the store from the index behind the write hook's back
    /// and prove all three shapes of disagreement are caught.
    #[test]
    fn drift_counts_every_way_an_entry_can_disagree_with_its_row() {
        let mut store = Store::new();
        // agrees
        store.hset(b"u:1", &[(b"age".as_slice(), b"30".as_slice())]).unwrap();
        // disagrees — the row says 41, the index holds 40
        store.hset(b"u:2", &[(b"age".as_slice(), b"41".as_slice())]).unwrap();
        // gone — no row at all, but the index still holds the key
        // (u:3 deliberately not written)

        let entries = vec![
            (b"u:1".to_vec(), IndexValue::I64(30)),
            (b"u:2".to_vec(), IndexValue::I64(40)),
            (b"u:3".to_vec(), IndexValue::I64(50)),
        ];
        let chunk = encode_verify_chunk(&mut store, &spec(), &entries, &stats());
        let (drift, checked) = drift_and_checked(&chunk);
        assert_eq!(checked, 3, "every held entry must be re-read");
        assert_eq!(drift, 2, "the changed row and the missing row must both count");
    }

    #[test]
    fn a_healthy_index_reports_zero_drift() {
        let mut store = Store::new();
        store.hset(b"u:1", &[(b"age".as_slice(), b"30".as_slice())]).unwrap();
        store.hset(b"u:2", &[(b"age".as_slice(), b"40".as_slice())]).unwrap();
        let entries = vec![
            (b"u:1".to_vec(), IndexValue::I64(30)),
            (b"u:2".to_vec(), IndexValue::I64(40)),
        ];
        let chunk = encode_verify_chunk(&mut store, &spec(), &entries, &stats());
        assert_eq!(drift_and_checked(&chunk), (0, 2));
    }

    /// A row whose field stopped coercing (someone wrote a string into an i64
    /// index's field) is drift, not silence.
    #[test]
    fn a_row_that_no_longer_coerces_counts_as_drift() {
        let mut store = Store::new();
        store.hset(b"u:1", &[(b"age".as_slice(), b"not-a-number".as_slice())]).unwrap();
        let entries = vec![(b"u:1".to_vec(), IndexValue::I64(30))];
        let chunk = encode_verify_chunk(&mut store, &spec(), &entries, &stats());
        assert_eq!(drift_and_checked(&chunk), (1, 1));
    }
}
