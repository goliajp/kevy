//! Write-path index maintenance — the `commit_write` hook body plus
//! the catalog↔shard segment-list reconciliation (split out of
//! `ops_index.rs` to keep it under the 500-LOC project ceiling;
//! behaviour unchanged).

use kevy_index::{IndexKind, IndexSpec, IndexValue, Segment};

use crate::ops_index::{IndexReg, ShardSegs};

/// A fresh text segment shaped by the spec: as many separately scored
/// fields as it declares (the breakdown `IN <field…>` scopes to), and a
/// positional side-channel when it asked for `WITH POSITIONS`.
#[cfg(feature = "text")]
pub(crate) fn new_text(spec: &IndexSpec) -> kevy_text::TextSegment {
    kevy_text::TextSegment::with_shape(kevy_text::SegmentShape {
        fields: spec.fields.len(),
        positions: spec.with_positions,
        values: spec.values.len(),
    })
}

#[cfg(feature = "vector")]
pub(crate) fn new_graph(spec: &IndexSpec) -> kevy_vector::Hnsw {
    let a = spec.ann.as_ref().expect("ann spec");
    kevy_vector::Hnsw::new(
        a.dim as usize,
        kevy_vector::HnswParams {
            m: a.m as usize,
            ef_construction: a.ef as usize,
            distance: match a.distance {
                1 => kevy_vector::Distance::L2,
                2 => kevy_vector::Distance::Ip,
                _ => kevy_vector::Distance::Cosine,
            },
        },
    )
}

/// Reconcile one shard's segment list with the catalog; new indexes
/// backfill from this shard's live keys (we hold the shard's write
/// lock — no concurrent writes can race the scan).
pub(crate) fn sync_segs(
    reg: &IndexReg,
    shard_segs: &mut ShardSegs,
    store: &mut kevy_store::Store,
) {
    let g = reg
        .catalog
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (ver, cat) = &*g;
    if shard_segs.version == *ver {
        return;
    }
    rebuild_seg_lists(cat, *ver, shard_segs, store);
}

/// The out-of-date half of `sync_segs`: rebuild every per-kind segment
/// list against the catalog (existing segments move, new specs backfill).
fn rebuild_seg_lists(
    cat: &kevy_index::Catalog,
    ver: u64,
    shard_segs: &mut ShardSegs,
    store: &mut kevy_store::Store,
) {
    let mut next: Vec<(IndexSpec, Segment)> = Vec::new();
    #[cfg(feature = "text")]
    let mut next_text: Vec<(IndexSpec, kevy_text::TextSegment)> = Vec::new();
    #[cfg(feature = "vector")]
    let mut next_ann: Vec<(IndexSpec, kevy_vector::Hnsw)> = Vec::new();
    let mut next_agg: Vec<(IndexSpec, kevy_index::AggSegment)> = Vec::new();
    for (spec, _) in cat.iter() {
        let (segs, st) = (&mut *shard_segs, &mut *store);
        match spec.kind {
            IndexKind::Agg => next_agg
                .push(take_or_backfill(&mut segs.agg, spec, st, kevy_index::AggSegment::new, apply_agg_key)),
            #[cfg(feature = "vector")]
            IndexKind::Ann => next_ann
                .push(take_or_backfill(&mut segs.ann, spec, st, || new_graph(spec), apply_ann_key)),
            // Engine compiled out (idx_create rejects the kind; a
            // sidecar-loaded spec gets no segment, so queries answer
            // NotFound instead of silently mis-indexing).
            #[cfg(not(feature = "vector"))]
            IndexKind::Ann => {}
            #[cfg(feature = "text")]
            IndexKind::Text => next_text
                .push(take_or_backfill(&mut segs.text, spec, st, || new_text(spec), apply_text_key)),
            #[cfg(not(feature = "text"))]
            IndexKind::Text => {}
            _ => next.push(take_or_backfill(&mut segs.segs, spec, st, Segment::new, apply_key)),
        }
    }
    shard_segs.segs = next;
    #[cfg(feature = "text")]
    {
        shard_segs.text = next_text;
    }
    #[cfg(feature = "vector")]
    {
        shard_segs.ann = next_ann;
    }
    shard_segs.agg = next_agg;
    shard_segs.version = ver;
}

/// Keep `spec`'s existing segment from `have` (position move), or
/// backfill a fresh one from this shard's live keys in the spec's
/// prefix domain.
fn take_or_backfill<S>(
    have: &mut Vec<(IndexSpec, S)>,
    spec: &IndexSpec,
    store: &mut kevy_store::Store,
    empty: impl FnOnce() -> S,
    apply: impl Fn(&mut kevy_store::Store, &IndexSpec, &mut S, &[u8]),
) -> (IndexSpec, S) {
    if let Some(i) = have.iter().position(|(s, _)| s == spec) {
        return have.swap_remove(i);
    }
    let mut seg = empty();
    let mut pat = spec.prefix.clone();
    pat.push(b'*');
    for key in store.collect_keys(Some(&pat), None) {
        apply(store, spec, &mut seg, &key);
    }
    (spec.clone(), seg)
}

fn apply_agg_key(
    store: &mut kevy_store::Store,
    spec: &IndexSpec,
    a: &mut kevy_index::AggSegment,
    key: &[u8],
) {
    let group_field = spec.group_by.as_deref().unwrap_or_default();
    let group = match store.hget(key, group_field) {
        Ok(Some(g)) => Some(g.to_vec()),
        _ => None,
    };
    let val = match store.hget(key, spec.field()) {
        Ok(Some(raw)) => {
            let raw = raw.to_vec();
            kevy_index::IndexValue::coerce(spec.ty, &raw)
        }
        _ => None,
    };
    match (group, val) {
        (Some(g), Some(v)) => a.apply(key, Some((g, v)), false),
        _ => a.apply(key, None, store.exists(&[key]) > 0),
    }
}

#[cfg(feature = "vector")]
fn apply_ann_key(
    store: &mut kevy_store::Store,
    spec: &IndexSpec,
    g: &mut kevy_vector::Hnsw,
    key: &[u8],
) {
    let v = match store.hget(key, spec.field()) {
        Ok(Some(raw)) => {
            let raw = raw.to_vec();
            kevy_vector::parse_vector(&raw, g.dim())
        }
        _ => None,
    };
    g.apply(key, v);
}

#[cfg(feature = "text")]
fn apply_text_key(
    store: &mut kevy_store::Store,
    spec: &IndexSpec,
    ts: &mut kevy_text::TextSegment,
    key: &[u8],
) {
    // The spec owns what it reads out of a row -- declared fields with
    // their weights, declared stored values -- so this path and the
    // server's cannot index the same row differently.
    let (fields, values) = spec.read_row(|f| store.hget(key, f).ok().flatten().map(|v| v.to_vec()));
    let vals: Vec<Option<&[u8]>> = values.iter().map(|v| v.as_deref()).collect();
    if fields.is_empty() {
        ts.apply_doc(key, None, &vals);
    } else {
        ts.apply_doc(key, Some(&fields), &vals);
    }
}

/// The write-path hook body — called from `commit_write` with the
/// logged argv, under the shard lock. Extracts the written key(s)
/// EXACTLY and re-derives their index entries.
pub(crate) fn on_commit(
    reg: &IndexReg,
    shard_segs: &mut ShardSegs,
    store: &mut kevy_store::Store,
    parts: &[&[u8]],
) {
    {
        // Cheap gate: empty catalog = one read-lock + is_empty.
        let g = reg
            .catalog
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if g.1.is_empty() {
            return;
        }
    }
    sync_segs(reg, shard_segs, store);
    let verb = parts.first().copied().unwrap_or(b"");
    if verb.eq_ignore_ascii_case(b"FLUSHALL") || verb.eq_ignore_ascii_case(b"FLUSHDB") {
        reset_all_segs(shard_segs);
        return;
    }
    each_written_key(verb, parts, |key| {
        for (spec, seg) in &mut shard_segs.segs {
            if key.starts_with(&spec.prefix) {
                apply_key(store, spec, seg, key);
            }
        }
        #[cfg(feature = "text")]
        for (spec, ts) in &mut shard_segs.text {
            if key.starts_with(&spec.prefix) {
                apply_text_key(store, spec, ts, key);
            }
        }
        #[cfg(feature = "vector")]
        for (spec, g) in &mut shard_segs.ann {
            if key.starts_with(&spec.prefix) {
                apply_ann_key(store, spec, g, key);
            }
        }
        for (spec, a) in &mut shard_segs.agg {
            if key.starts_with(&spec.prefix) {
                apply_agg_key(store, spec, a, key);
            }
        }
    });
}

/// FLUSHALL / FLUSHDB: every segment resets to empty.
fn reset_all_segs(shard_segs: &mut ShardSegs) {
    for (_, seg) in &mut shard_segs.segs {
        *seg = Segment::new();
    }
    #[cfg(feature = "text")]
    for (spec, ts) in &mut shard_segs.text {
        *ts = new_text(spec);
    }
    #[cfg(feature = "vector")]
    for (spec, g) in &mut shard_segs.ann {
        *g = new_graph(spec);
    }
    for (_, a) in &mut shard_segs.agg {
        *a = kevy_index::AggSegment::new();
    }
}

/// EXACT written-key walk per verb shape (the logged effect argv is a
/// closed set — new effect verbs must be added here; the parity test
/// in `store_tests_index.rs` pins the list).
pub(crate) fn each_written_key_pub(verb: &[u8], parts: &[&[u8]], f: impl FnMut(&[u8])) {
    each_written_key(verb, parts, f);
}

fn each_written_key(verb: &[u8], parts: &[&[u8]], mut f: impl FnMut(&[u8])) {
    let up = |v: &[u8], t: &[u8]| v.eq_ignore_ascii_case(t);
    if up(verb, b"DEL") || up(verb, b"UNLINK") {
        for k in &parts[1..] {
            f(k);
        }
    } else if up(verb, b"MSET") {
        let mut i = 1;
        while i + 1 < parts.len() {
            f(parts[i]);
            i += 2;
        }
    } else if up(verb, b"COPY") || up(verb, b"RENAME") || up(verb, b"RENAMENX") {
        if let Some(k) = parts.get(1) {
            f(k);
        }
        if let Some(k) = parts.get(2) {
            f(k);
        }
    } else if let Some(k) = parts.get(1) {
        f(k);
    }
}

fn apply_key(store: &mut kevy_store::Store, spec: &IndexSpec, seg: &mut Segment, key: &[u8]) {
    match store.hget(key, spec.field()) {
        Ok(Some(raw)) => {
            let raw = raw.to_vec();
            match IndexValue::coerce(spec.ty, &raw) {
                Some(v) => seg.apply(key, Some(v)),
                None => seg.apply(key, None),
            }
        }
        Ok(None) => {
            if store.exists(&[key]) == 0 {
                seg.remove(key);
            } else {
                seg.apply(key, None);
            }
        }
        Err(_) => seg.remove(key),
    }
}
