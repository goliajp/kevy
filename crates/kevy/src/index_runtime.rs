//! v2.5 — the index engine's runtime half (RFC LOCKED 2026-07-04).
//!
//! Topology: one process-global [`Catalog`] behind an RwLock +
//! generation counter; each shard keeps its [`ShardIndexes`] (its
//! slice of every index — index-follows-key) in `ShardCtx.indexes`,
//! refreshed lazily when the generation moves. The write path enters
//! through [`on_write`] (wired to `Commands::on_write`), which the
//! caller gates on the `IDX_NONEMPTY` gate bit (K-103 W5) — the RFC
//! D2 zero-tax posture: an empty catalog costs one cached-bit branch.
//!
//! Backfill (RFC D5, tick-incremental variant): `IDX.CREATE` snapshots
//! the domain's key list per shard; `on_shard_tick` indexes a bounded
//! batch per tick until exhausted (non-blocking, no extra threads,
//! shard-affine). Live writes during the build hit the hook first and
//! win: the backfill only fills keys the segment doesn't hold yet, so
//! a newer hook-applied value is never clobbered by a stale scan.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use kevy_index::{Catalog, IndexSpec, IndexValue, Segment};
use kevy_store::Store;

use crate::state::ShardCtx;

static CATALOG_GEN: AtomicU64 = AtomicU64::new(0);
static CATALOG: RwLock<Option<Arc<Catalog>>> = RwLock::new(None);

/// Per-shard build progress for one index.
enum BuildState {
    /// Keys captured at create-time, next position to process.
    Backfilling { keys: Vec<Vec<u8>>, pos: usize },
    /// Serving.
    Ready,
    /// Build crossed the spec's MAXMEM budget (RFC D7): declarative
    /// failure, queries answer an error, no OOM.
    FailedOverBudget,
}

struct ShardIndex {
    spec: IndexSpec,
    seg: Segment,
    /// v2.7: populated instead of `seg` for KIND text.
    text: Option<kevy_text::TextSegment>,
    /// v2.8: populated instead of `seg` for KIND ann.
    ann: Option<kevy_vector::Hnsw>,
    /// v3.1: populated instead of `seg` for KIND agg.
    agg: Option<kevy_index::AggSegment>,
    build: BuildState,
}

/// One shard's slice of every declared index. Owned by
/// `crate::state::ShardCtx` (W4); every entry point below borrows it
/// from the caller's shard zone.
#[derive(Default)]
pub(crate) struct ShardIndexes {
    generation: u64,
    idx: Vec<ShardIndex>,
}

/// Snapshot the current catalog (None = empty).
pub(crate) fn catalog() -> Option<Arc<Catalog>> {
    CATALOG
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Is at least one index declared? Cold-path input to the per-shard
/// `IDX_NONEMPTY` gate bit — the hot path reads the cached bit, never
/// this lock.
pub(crate) fn catalog_nonempty() -> bool {
    CATALOG
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .is_some_and(|c| !c.is_empty())
}

/// Swap in a new catalog version (IDX.CREATE / IDX.DROP). Bumps the
/// generation (shards refresh their segment lists lazily), then the
/// control epoch (writer protocol step ② — every shard's gate bits
/// re-derive `IDX_NONEMPTY` on their next command).
pub(crate) fn install_catalog(control_epoch: &AtomicU64, c: Catalog) {
    *CATALOG
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(c));
    CATALOG_GEN.fetch_add(1, Ordering::Release);
    control_epoch.fetch_add(1, Ordering::Release);
}

/// The write-path hook body (`Commands::on_write`). The caller gates
/// on `IDX_NONEMPTY`, so entering here means at least one index is
/// declared.
#[inline]
pub(crate) fn on_write(shard: &ShardCtx, store: &mut Store, key: &[u8]) {
    let mut st = shard.indexes.borrow_mut();
    refresh(&mut st, store);
    for si in &mut st.idx {
        if key.starts_with(&si.spec.prefix) {
            apply_row(store, si, key);
        }
    }
}

/// Tick hook: advance backfills a bounded batch per tick. Gated like
/// [`on_write`].
pub(crate) fn on_tick(shard: &ShardCtx, store: &mut Store) {
    let mut st = shard.indexes.borrow_mut();
    refresh(&mut st, store);
    for si in &mut st.idx {
        advance_backfill(store, si, 2048);
    }
}

/// Query entry: run `f` against this shard's segment for `name`.
/// `None` = index unknown here (a stale shard list is refreshed
/// first) or still backfilling. Wired to IDX.QUERY fan-out in step 2b.
pub(crate) fn with_ready_segment<R>(
    shard: &ShardCtx,
    store: &mut Store,
    name: &[u8],
    f: impl FnOnce(&IndexSpec, &Segment) -> R,
) -> Result<R, &'static str> {
    let mut st = shard.indexes.borrow_mut();
    refresh(&mut st, store);
    let si = st
        .idx
        .iter()
        .find(|si| si.spec.name == name)
        .ok_or("ERR no such index")?;
    match si.build {
        BuildState::Ready => Ok(f(&si.spec, &si.seg)),
        BuildState::Backfilling { .. } => Err("INDEXBUILDING index is still building"),
        BuildState::FailedOverBudget => Err("INDEXOVERBUDGET index build exceeded MAXMEM"),
    }
}

/// v3.1: run `f` against a READY aggregate segment.
pub(crate) fn with_ready_agg<R>(
    shard: &ShardCtx,
    store: &mut Store,
    name: &[u8],
    f: impl FnOnce(&kevy_index::AggSegment) -> R,
) -> Result<R, &'static str> {
    let mut st = shard.indexes.borrow_mut();
    refresh(&mut st, store);
    let si = st
        .idx
        .iter()
        .find(|si| si.spec.name == name)
        .ok_or("ERR no such index")?;
    match (&si.build, &si.agg) {
        (BuildState::Ready, Some(a)) => Ok(f(a)),
        (BuildState::Backfilling { .. }, _) => Err("INDEXBUILDING index is still building"),
        (BuildState::FailedOverBudget, _) => Err("INDEXOVERBUDGET index build exceeded MAXMEM"),
        (_, None) => Err("ERR not an aggregate index"),
    }
}

/// v2.8: run `f` against a READY ANN graph (mutable for REBUILD).
pub(crate) fn with_ready_ann<R>(
    shard: &ShardCtx,
    store: &mut Store,
    name: &[u8],
    f: impl FnOnce(&mut kevy_vector::Hnsw) -> R,
) -> Result<R, &'static str> {
    let mut st = shard.indexes.borrow_mut();
    refresh(&mut st, store);
    let si = st
        .idx
        .iter_mut()
        .find(|si| si.spec.name == name)
        .ok_or("ERR no such index")?;
    match (&si.build, &mut si.ann) {
        (BuildState::Ready, Some(g)) => Ok(f(g)),
        (BuildState::Backfilling { .. }, _) => Err("INDEXBUILDING index is still building"),
        (BuildState::FailedOverBudget, _) => Err("INDEXOVERBUDGET index build exceeded MAXMEM"),
        (_, None) => Err("ERR not a vector index"),
    }
}

/// v2.7: run `f` against a READY text segment.
pub(crate) fn with_ready_text_segment<R>(
    shard: &ShardCtx,
    store: &mut Store,
    name: &[u8],
    f: impl FnOnce(&kevy_text::TextSegment) -> R,
) -> Result<R, &'static str> {
    let mut st = shard.indexes.borrow_mut();
    refresh(&mut st, store);
    let si = st
        .idx
        .iter()
        .find(|si| si.spec.name == name)
        .ok_or("ERR no such index")?;
    match (&si.build, &si.text) {
        (BuildState::Ready, Some(ts)) => Ok(f(ts)),
        (BuildState::Backfilling { .. }, _) => Err("INDEXBUILDING index is still building"),
        (BuildState::FailedOverBudget, _) => Err("INDEXOVERBUDGET index build exceeded MAXMEM"),
        (_, None) => Err("ERR not a text index"),
    }
}

/// v2.6: run `f` with a name→segment resolver over this shard's READY
/// segments (views probe several indexes per call). Building/failed
/// segments resolve to None.
pub(crate) fn with_segment_resolver<R>(
    shard: &ShardCtx,
    store: &mut Store,
    f: impl for<'s> FnOnce(&'s dyn Fn(&[u8]) -> Option<&'s Segment>) -> R,
) -> R {
    let mut st = shard.indexes.borrow_mut();
    refresh(&mut st, store);
    let idx = &st.idx;
    let resolver = |name: &[u8]| -> Option<&Segment> {
        idx.iter()
            .find(|si| si.spec.name == name && matches!(si.build, BuildState::Ready))
            .map(|si| &si.seg)
    };
    f(&resolver)
}

/// Two-segment variant for COMPOSE — one RefCell borrow (nesting
/// [`with_ready_segment`] would double-borrow the shard's index list).
pub(crate) fn with_two_ready_segments<R>(
    shard: &ShardCtx,
    store: &mut Store,
    a: &[u8],
    b: &[u8],
    f: impl FnOnce(&IndexSpec, &Segment, &IndexSpec, &Segment) -> R,
) -> Result<R, &'static str> {
    let mut st = shard.indexes.borrow_mut();
    refresh(&mut st, store);
    let ia = st.idx.iter().position(|si| si.spec.name == a).ok_or("ERR no such index")?;
    let ib = st.idx.iter().position(|si| si.spec.name == b).ok_or("ERR no such index")?;
    for i in [ia, ib] {
        if matches!(st.idx[i].build, BuildState::Backfilling { .. }) {
            return Err("INDEXBUILDING index is still building");
        }
    }
    let (sa, sb) = (&st.idx[ia], &st.idx[ib]);
    Ok(f(&sa.spec, &sa.seg, &sb.spec, &sb.seg))
}

/// Whether this shard's slice of `name` is still backfilling.
pub(crate) fn segment_building(shard: &ShardCtx, store: &mut Store, name: &[u8]) -> bool {
    let mut st = shard.indexes.borrow_mut();
    refresh(&mut st, store);
    st.idx
        .iter()
        .find(|si| si.spec.name == name)
        .is_some_and(|si| matches!(si.build, BuildState::Backfilling { .. }))
}

/// Reconcile this shard's segment list with the global catalog:
/// keep segments whose spec is unchanged, start backfills for new
/// ones, drop removed ones.
fn refresh(st: &mut ShardIndexes, store: &mut Store) {
    let generation = CATALOG_GEN.load(Ordering::Acquire);
    if st.generation == generation {
        return;
    }
    let cat = catalog();
    let mut next: Vec<ShardIndex> = Vec::new();
    if let Some(cat) = cat {
        for (spec, _state) in cat.iter() {
            match st.idx.iter().position(|si| si.spec == *spec) {
                Some(i) => next.push(st.idx.swap_remove(i)),
                None => {
                    // Snapshot the domain's keys on THIS shard; live
                    // writes from now on hit the hook first and win.
                    let mut pat = spec.prefix.clone();
                    pat.push(b'*');
                    let keys = store.collect_keys(Some(&pat), None);
                    next.push(ShardIndex {
                        agg: (spec.kind == kevy_index::IndexKind::Agg)
                            .then(kevy_index::AggSegment::new),
                        text: (spec.kind == kevy_index::IndexKind::Text)
                            .then(kevy_text::TextSegment::new),
                        ann: spec.ann.as_ref().map(|a| {
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
                        }),
                        spec: spec.clone(),
                        seg: Segment::new(),
                        build: BuildState::Backfilling { keys, pos: 0 },
                    });
                }
            }
        }
    }
    st.idx = next;
    st.generation = generation;
}

/// Index one row: read the field from the hash at `key`, coerce,
/// apply. A missing key / non-hash / missing field clears the row.
fn apply_row(store: &mut Store, si: &mut ShardIndex, key: &[u8]) {
    // v3.1 agg kind: both fields must resolve — the aggregated value
    // coerces per the declared type, the group key is raw bytes.
    if let Some(a) = &mut si.agg {
        apply_row_agg(store, &si.spec, a, key);
        return;
    }
    // v2.8 ann kind: field bytes parse as an f32 vector (wrong shape
    // = excluded, same discipline as scalar coerce failure).
    if let Some(g) = &mut si.ann {
        let v = match store.hget(key, &si.spec.field) {
            Ok(Some(raw)) => {
                let raw = raw.to_vec();
                kevy_vector::parse_vector(&raw, g.dim())
            }
            _ => None,
        };
        g.apply(key, v);
        return;
    }
    // v2.7 text kind: raw field bytes tokenize into the inverted
    // segment (no scalar coercion).
    if let Some(ts) = &mut si.text {
        match store.hget(key, &si.spec.field) {
            Ok(Some(raw)) => {
                let raw = raw.to_vec();
                ts.apply(key, Some(&raw));
            }
            _ => ts.apply(key, None),
        }
        return;
    }
    let val = row_value(store, &si.spec, key);
    match val {
        RowValue::Value(v) => si.seg.apply(key, Some(v)),
        RowValue::CoerceFailed => si.seg.apply(key, None),
        RowValue::Gone => si.seg.remove(key),
    }
}

/// [`apply_row`]'s agg half: both fields must resolve — the aggregated
/// value coerces per the declared type, the group key is raw bytes.
fn apply_row_agg(
    store: &mut Store,
    spec: &IndexSpec,
    a: &mut kevy_index::AggSegment,
    key: &[u8],
) {
    let group_field = spec.group_by.as_deref().unwrap_or_default();
    let group = match store.hget(key, group_field) {
        Ok(Some(g)) => Some(g.to_vec()),
        _ => None,
    };
    let val = match store.hget(key, &spec.field) {
        Ok(Some(raw)) => {
            let raw = raw.to_vec();
            kevy_index::IndexValue::coerce(spec.ty, &raw)
        }
        _ => None,
    };
    match (group, val) {
        (Some(g), Some(v)) => a.apply(key, Some((g, v)), false),
        // Slow path only: distinguish a DELETED row (plain
        // retract) from an in-domain row missing/failing a field
        // (excluded, counted). The happy path above never pays
        // the exists() probe.
        _ => a.apply(key, None, store.exists(&[key.to_vec()]) > 0),
    }
}

enum RowValue {
    Value(IndexValue),
    CoerceFailed,
    Gone,
}

fn row_value(store: &mut Store, spec: &IndexSpec, key: &[u8]) -> RowValue {
    match store.hget(key, &spec.field) {
        Ok(Some(raw)) => {
            let raw = raw.to_vec();
            match IndexValue::coerce(spec.ty, &raw) {
                Some(v) => RowValue::Value(v),
                None => RowValue::CoerceFailed,
            }
        }
        // `hget` answers None for BOTH a missing key and a missing
        // field; only the latter is a row excluded by coercion — a
        // missing key is simply not a row.
        Ok(None) => {
            if store.exists(&[key.to_vec()]) == 0 {
                RowValue::Gone
            } else {
                RowValue::CoerceFailed
            }
        }
        Err(_) => RowValue::Gone, // not a hash → not a row
    }
}

fn advance_backfill(store: &mut Store, si: &mut ShardIndex, batch: usize) {
    let BuildState::Backfilling { keys, pos } = &mut si.build else {
        return;
    };
    let end = (*pos + batch).min(keys.len());
    // Split the borrow: take the key slice out while applying.
    let slice: Vec<Vec<u8>> = keys[*pos..end].to_vec();
    *pos = end;
    let done = *pos >= keys.len();
    for key in &slice {
        // Hook-applied entries win: only fill keys not yet indexed.
        let already = match (&si.text, &si.ann, &si.agg) {
            (Some(ts), _, _) => ts.contains(key),
            (_, Some(g), _) => g.contains(key),
            (_, _, Some(a)) => a.contains(key),
            _ => si.seg.verify_entry(key).is_some(),
        };
        if !already {
            apply_row_backfill(store, si, key);
        }
    }
    // RFC D7: a MAXMEM budget is enforced at build time —
    // declarative failure instead of OOM.
    if si.spec.max_bytes > 0 && si.seg.stats().approx_bytes > si.spec.max_bytes {
        si.seg = Segment::new();
        si.build = BuildState::FailedOverBudget;
        return;
    }
    if done {
        si.build = BuildState::Ready;
    }
}

fn apply_row_backfill(store: &mut Store, si: &mut ShardIndex, key: &[u8]) {
    if si.text.is_some() || si.ann.is_some() || si.agg.is_some() {
        apply_row(store, si, key);
        return;
    }
    match row_value(store, &si.spec, key) {
        RowValue::Value(v) => si.seg.apply(key, Some(v)),
        RowValue::CoerceFailed => si.seg.apply(key, None),
        RowValue::Gone => {} // deleted since snapshot — nothing to do
    }
}

#[cfg(test)]
mod tests;
