//! The index engine's runtime half.
//!
//! Topology: one process-wide [`Catalog`] behind an RwLock +
//! generation counter, both owned by `RuntimeState.catalogs`;
//! each shard keeps its [`ShardIndexes`] (its slice of
//! every index — index-follows-key) in `ShardCtx.indexes`, refreshed
//! lazily when the generation moves. The write path enters through
//! [`on_write`] (wired to `Commands::on_write`), which the caller
//! gates on the `IDX_NONEMPTY` gate bit — the
//! zero-tax posture: an empty catalog costs one cached-bit branch.
//!
//! Backfill (tick-incremental variant): `IDX.CREATE` snapshots
//! the domain's key list per shard; `on_shard_tick` indexes a bounded
//! batch per tick until exhausted (non-blocking, no extra threads,
//! shard-affine). Live writes during the build hit the hook first and
//! win: the backfill only fills keys the segment doesn't hold yet, so
//! a newer hook-applied value is never clobbered by a stale scan.

use kevy_resp::CmdError;
use kevy_index::{IndexSpec, Segment};
use kevy_store::Store;

use crate::state::{CatalogState, Ctx};

/// Per-shard build progress for one index.
enum BuildState {
    /// Keys captured at create-time, next position to process.
    Backfilling { keys: Vec<Vec<u8>>, pos: usize },
    /// Serving.
    Ready,
    /// Build crossed the spec's MAXMEM budget: declarative
    /// failure, queries answer an error, no OOM.
    FailedOverBudget,
}

struct ShardIndex {
    spec: IndexSpec,
    seg: Segment,
    /// Populated instead of `seg` for KIND text.
    text: Option<kevy_text::TextSegment>,
    /// Populated instead of `seg` for KIND ann.
    ann: Option<kevy_vector::Hnsw>,
    /// Populated instead of `seg` for KIND agg.
    agg: Option<kevy_index::AggSegment>,
    build: BuildState,
}

/// One shard's slice of every declared index. Owned by
/// `crate::state::ShardCtx`; every entry point below borrows it
/// from the caller's shard zone.
#[derive(Default)]
pub(crate) struct ShardIndexes {
    generation: u64,
    idx: Vec<ShardIndex>,
    /// v4.1-V5 `reserved_bytes` generation cache: set by every
    /// segment-mutating chokepoint (write applies, backfill batches,
    /// catalog refresh); an idle tick reads the cached sum instead of
    /// walking every segment's stats — the walk behind the sum was
    /// mailrs's measured 300-500× idle-CPU term (F16a).
    stats_dirty: bool,
    reserved_cache: u64,
}

/// The write-path hook body (`Commands::on_write`). The caller gates
/// on `IDX_NONEMPTY`, so entering here means at least one index is
/// declared.
#[inline]
pub(crate) fn on_write(ctx: &Ctx<'_>, store: &mut Store, key: &[u8]) {
    let mut st = ctx.shard.indexes.borrow_mut();
    refresh(&ctx.state.catalogs, &mut st, store);
    let st = &mut *st;
    for si in &mut st.idx {
        if key.starts_with(&si.spec.prefix) {
            apply_row(store, si, key);
            st.stats_dirty = true;
        }
    }
}

/// Tick hook: advance backfills a bounded batch per tick. Gated like
/// [`on_write`].
pub(crate) fn on_tick(ctx: &Ctx<'_>, store: &mut Store) {
    let mut st = ctx.shard.indexes.borrow_mut();
    refresh(&ctx.state.catalogs, &mut st, store);
    let st = &mut *st;
    for si in &mut st.idx {
        if matches!(si.build, BuildState::Backfilling { .. }) {
            st.stats_dirty = true;
        }
        advance_backfill(store, si, 2048);
    }
}

/// Σ approximate heap bytes of this shard's index segments, every
/// kind (scalar / text / ann / agg) — the tier's `reserved_bytes`
/// floor feed. Called per shard tick, gated on
/// tiering being enabled; refreshes the shard list first so a
/// just-declared index counts immediately.
/// FLUSHALL/FLUSHDB emptied this shard's store: every segment resets
/// to its declared-empty shape (found stale during v4.1-V5 — the
/// embedded face's `on_commit` reset on FLUSH; this face kept serving
/// deleted keys out of IDX.QUERY). A mid-backfill index goes straight
/// to Ready: its snapshot's keys no longer exist.
pub(crate) fn on_flush(ctx: &Ctx<'_>, store: &mut Store) {
    let mut st = ctx.shard.indexes.borrow_mut();
    refresh(&ctx.state.catalogs, &mut st, store);
    let st = &mut *st;
    for si in &mut st.idx {
        si.seg = new_scalar_seg(&si.spec);
        si.text = new_text_seg(&si.spec);
        si.ann = new_ann_seg(&si.spec);
        si.agg = (si.spec.kind == kevy_index::IndexKind::Agg).then(kevy_index::AggSegment::new);
        si.build = BuildState::Ready;
        st.stats_dirty = true;
    }
}

/// Served from the generation cache: an idle store recomputes
/// nothing (v4.1-V5).
pub(crate) fn reserved_bytes(ctx: &Ctx<'_>, store: &mut Store) -> u64 {
    let mut st = ctx.shard.indexes.borrow_mut();
    refresh(&ctx.state.catalogs, &mut st, store);
    if !st.stats_dirty {
        return st.reserved_cache;
    }
    let sum = st
        .idx
        .iter()
        .map(|si| {
            si.seg.stats().approx_bytes
                + si.text.as_ref().map_or(0, |t| t.stats().approx_bytes)
                + si.ann.as_ref().map_or(0, |g| g.stats().approx_bytes)
                + si.agg.as_ref().map_or(0, |a| a.stats().approx_bytes)
        })
        .sum();
    st.reserved_cache = sum;
    st.stats_dirty = false;
    sum
}

/// Query entry: run `f` against this shard's segment for `name`.
/// `None` = index unknown here (a stale shard list is refreshed
/// first) or still backfilling. Wired to IDX.QUERY fan-out in step 2b.
pub(crate) fn with_ready_segment<R>(
    ctx: &Ctx<'_>,
    store: &mut Store,
    name: &[u8],
    f: impl FnOnce(&IndexSpec, &Segment) -> R,
) -> Result<R, CmdError> {
    let mut st = ctx.shard.indexes.borrow_mut();
    refresh(&ctx.state.catalogs, &mut st, store);
    let si = st
        .idx
        .iter()
        .find(|si| si.spec.name == name)
        .ok_or("ERR no such index")?;
    match si.build {
        BuildState::Ready => Ok(f(&si.spec, &si.seg)),
        BuildState::Backfilling { .. } => Err(CmdError::Wire("INDEXBUILDING index is still building")),
        BuildState::FailedOverBudget => Err(CmdError::Wire("INDEXOVERBUDGET index build exceeded MAXMEM")),
    }
}

/// Run `f` against a READY aggregate segment.
pub(crate) fn with_ready_agg<R>(
    ctx: &Ctx<'_>,
    store: &mut Store,
    name: &[u8],
    f: impl FnOnce(&kevy_index::AggSegment) -> R,
) -> Result<R, CmdError> {
    let mut st = ctx.shard.indexes.borrow_mut();
    refresh(&ctx.state.catalogs, &mut st, store);
    let si = st
        .idx
        .iter()
        .find(|si| si.spec.name == name)
        .ok_or("ERR no such index")?;
    match (&si.build, &si.agg) {
        (BuildState::Ready, Some(a)) => Ok(f(a)),
        (BuildState::Backfilling { .. }, _) => Err(CmdError::Wire("INDEXBUILDING index is still building")),
        (BuildState::FailedOverBudget, _) => Err(CmdError::Wire("INDEXOVERBUDGET index build exceeded MAXMEM")),
        (_, None) => Err(CmdError::Wire("ERR not an aggregate index")),
    }
}

/// Run `f` against a READY ANN graph (mutable for REBUILD).
pub(crate) fn with_ready_ann<R>(
    ctx: &Ctx<'_>,
    store: &mut Store,
    name: &[u8],
    f: impl FnOnce(&mut kevy_vector::Hnsw) -> R,
) -> Result<R, CmdError> {
    let mut st = ctx.shard.indexes.borrow_mut();
    refresh(&ctx.state.catalogs, &mut st, store);
    let si = st
        .idx
        .iter_mut()
        .find(|si| si.spec.name == name)
        .ok_or("ERR no such index")?;
    match (&si.build, &mut si.ann) {
        (BuildState::Ready, Some(g)) => Ok(f(g)),
        (BuildState::Backfilling { .. }, _) => Err(CmdError::Wire("INDEXBUILDING index is still building")),
        (BuildState::FailedOverBudget, _) => Err(CmdError::Wire("INDEXOVERBUDGET index build exceeded MAXMEM")),
        (_, None) => Err(CmdError::Wire("ERR not a vector index")),
    }
}

/// Run `f` against a READY text segment.
pub(crate) fn with_ready_text_segment<R>(
    ctx: &Ctx<'_>,
    store: &mut Store,
    name: &[u8],
    f: impl FnOnce(&kevy_text::TextSegment, &kevy_index::IndexSpec) -> R,
) -> Result<R, CmdError> {
    let mut st = ctx.shard.indexes.borrow_mut();
    refresh(&ctx.state.catalogs, &mut st, store);
    let si = st
        .idx
        .iter()
        .find(|si| si.spec.name == name)
        .ok_or("ERR no such index")?;
    match (&si.build, &si.text) {
        (BuildState::Ready, Some(ts)) => Ok(f(ts, &si.spec)),
        (BuildState::Backfilling { .. }, _) => Err(CmdError::Wire("INDEXBUILDING index is still building")),
        (BuildState::FailedOverBudget, _) => Err(CmdError::Wire("INDEXOVERBUDGET index build exceeded MAXMEM")),
        (_, None) => Err(CmdError::Wire("ERR not a text index")),
    }
}

/// Run `f` with a name→segment resolver over this shard's READY
/// segments (views probe several indexes per call). Building/failed
/// segments resolve to None.
pub(crate) fn with_segment_resolver<R>(
    ctx: &Ctx<'_>,
    store: &mut Store,
    f: impl for<'s> FnOnce(&'s dyn Fn(&[u8]) -> Option<&'s Segment>) -> R,
) -> R {
    let mut st = ctx.shard.indexes.borrow_mut();
    refresh(&ctx.state.catalogs, &mut st, store);
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
    ctx: &Ctx<'_>,
    store: &mut Store,
    a: &[u8],
    b: &[u8],
    f: impl FnOnce(&IndexSpec, &Segment, &IndexSpec, &Segment) -> R,
) -> Result<R, CmdError> {
    let mut st = ctx.shard.indexes.borrow_mut();
    refresh(&ctx.state.catalogs, &mut st, store);
    let ia = st.idx.iter().position(|si| si.spec.name == a).ok_or("ERR no such index")?;
    let ib = st.idx.iter().position(|si| si.spec.name == b).ok_or("ERR no such index")?;
    for i in [ia, ib] {
        if matches!(st.idx[i].build, BuildState::Backfilling { .. }) {
            return Err(CmdError::Wire("INDEXBUILDING index is still building"));
        }
    }
    let (sa, sb) = (&st.idx[ia], &st.idx[ib]);
    Ok(f(&sa.spec, &sa.seg, &sb.spec, &sb.seg))
}

/// Whether this shard's slice of `name` is still backfilling.
pub(crate) fn segment_building(ctx: &Ctx<'_>, store: &mut Store, name: &[u8]) -> bool {
    let mut st = ctx.shard.indexes.borrow_mut();
    refresh(&ctx.state.catalogs, &mut st, store);
    st.idx
        .iter()
        .find(|si| si.spec.name == name)
        .is_some_and(|si| matches!(si.build, BuildState::Backfilling { .. }))
}

/// A fresh scalar segment for `spec` — with the stored-value
/// side-channel iff a scalar kind declared `VALUES` (text keeps its
/// values in the text segment; without the declaration this is the
/// plain `Segment::new()`, byte-identical to before — A5).
fn new_scalar_seg(spec: &IndexSpec) -> Segment {
    let scalar = matches!(spec.kind, kevy_index::IndexKind::Range | kevy_index::IndexKind::Unique);
    if scalar && !spec.values.is_empty() {
        Segment::with_values(spec.values.len())
    } else {
        Segment::new()
    }
}


/// A fresh text segment for `spec` when it is a text index — with the
/// positional side-channel iff it was created WITH POSITIONS.
/// A fresh HNSW graph shaped by the spec (None for non-ann kinds) —
/// shared by the catalog refresh and the FLUSH reset.
fn new_ann_seg(spec: &kevy_index::IndexSpec) -> Option<kevy_vector::Hnsw> {
    spec.ann.as_ref().map(|a| {
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
    })
}

fn new_text_seg(spec: &kevy_index::IndexSpec) -> Option<kevy_text::TextSegment> {
    (spec.kind == kevy_index::IndexKind::Text).then(|| {
        // The declared field count decides whether the segment keeps the
        // per-field breakdown `IN <field…>` scopes to; one field needs
        // none, because its per-field numbers are the merged ones.
        kevy_text::TextSegment::with_shape(kevy_text::SegmentShape {
            fields: spec.fields.len(),
            positions: spec.with_positions,
            values: spec.values.len(),
        })
    })
}

/// Reconcile this shard's segment list with the shared catalog:
/// keep segments whose spec is unchanged, start backfills for new
/// ones, drop removed ones.
fn refresh(catalogs: &CatalogState, st: &mut ShardIndexes, store: &mut Store) {
    let generation = catalogs.index_gen();
    if st.generation == generation {
        return;
    }
    st.stats_dirty = true;
    let cat = catalogs.index();
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
                        text: new_text_seg(spec),
                        ann: new_ann_seg(spec),
                        seg: new_scalar_seg(spec),
                        spec: spec.clone(),
                        build: BuildState::Backfilling { keys, pos: 0 },
                    });
                }
            }
        }
    }
    st.idx = next;
    st.generation = generation;
}

mod row_apply;
use row_apply::{advance_backfill, apply_row};
pub(crate) use row_apply::{RowValue, row_value};

#[cfg(test)]
mod tests;
