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
//! Backfill (tick-incremental variant): `IDX.CREATE` starts a cursor
//! walk over the domain's keys per shard; `on_shard_tick` indexes a
//! bounded batch per tick until the walk ends (non-blocking, no extra
//! threads, shard-affine). Live writes during the build hit the hook
//! first and win: the backfill only fills keys the segment doesn't hold
//! yet, so a newer hook-applied value is never clobbered by a stale scan.

use kevy_index::{IndexSpec, Segment};
use kevy_resp::CmdError;
use kevy_store::Store;

use crate::key_walk::KeyWalk;
use crate::state::{CatalogState, Ctx};

/// Per-shard build progress for one index.
#[derive(Debug)]
enum BuildState {
    /// Walking the domain's keys; the walk holds its cursor, not a copy
    /// of the keys.
    Backfilling(KeyWalk),
    /// Serving.
    Ready,
    /// Build crossed the spec's MAXMEM budget: declarative
    /// failure, queries answer an error, no OOM.
    FailedOverBudget,
}

#[derive(Debug)]
struct ShardIndex {
    spec: IndexSpec,
    seg: Segment,
    /// The sliding-window runtime — `Some` only when this index is a
    /// windowed table's single-column window access path.
    window: Option<kevy_window::WindowRt>,
    /// The text index's cold half — `Some` only when this is a
    /// windowed table's TEXT index.
    cold_text: Option<TextColdDir>,
    /// Populated instead of `seg` for KIND text.
    text: Option<kevy_text::TextSegment>,
    /// Populated instead of `seg` for KIND ann.
    ann: Option<kevy_vector::Hnsw>,
    /// Populated instead of `seg` for KIND agg.
    agg: Option<kevy_index::AggSegment>,
    /// This shard's part in a global index (its entries then live in the
    /// owned partitions, not in `seg`).
    global: Option<global::GlobalRole>,
    build: BuildState,
    /// Where this index's fields sit in the store's record of old rows.
    slots: Option<changes::Slots>,
}

/// One shard's slice of every declared index. Owned by
/// `crate::state::ShardCtx`; every entry point below borrows it
/// from the caller's shard zone.
#[derive(Debug, Default)]
pub(crate) struct ShardIndexes {
    generation: u64,
    idx: Vec<ShardIndex>,
    /// `reserved_bytes` generation cache: set by every
    /// segment-mutating chokepoint (write applies, backfill batches,
    /// catalog refresh); an idle tick reads the cached sum instead of
    /// walking every segment's stats — the walk behind the sum was
    /// a consumer's measured 300-500× idle-CPU term (F16a).
    stats_dirty: bool,
    reserved_cache: u64,
    /// Some index is global, so writes may queue deltas for other shards.
    any_global: bool,
    /// The last record taken from the store, lent back to the next take.
    spare: kevy_store::RowChanges,
    /// The watch rules installed in the store, and the index-list
    /// generation they were computed for.
    installed: changes::Rules,
    watch_gen: u64,
    /// The view catalog generation the key directories were set for.
    view_gen: u64,
}

/// The write-path hook body (`Commands::on_write`). The caller gates
/// on `IDX_NONEMPTY`, so entering here means at least one index is
/// declared.
#[inline]
pub(crate) fn on_write(ctx: &Ctx<'_>, store: &mut Store, _key: &[u8]) {
    let mut st = ctx.shard.indexes.borrow_mut();
    refresh(ctx, &mut st);
    // the store recorded every row written since the last drain, this key
    // among them, with what each held before
    changes::drain(store, &mut st);
}

/// Tick hook: advance backfills a bounded batch per tick, then slide
/// any windowed index whose boundary moved. Gated like [`on_write`].
pub(crate) fn on_tick(ctx: &Ctx<'_>, store: &mut Store) {
    let mut st = ctx.shard.indexes.borrow_mut();
    refresh(ctx, &mut st);
    // rows changed without a hook (a field expiring on a read) land here
    changes::drain(store, &mut st);
    let st = &mut *st;
    let segs_dir = shard_segs_dir(ctx.state, ctx.shard.shard_id());
    // Pass 1: backfills, then the scalar slide. The eviction batch's
    // keys (discovered on the window column's index) are kept so the
    // second pass can freeze the SAME batch out of the table's text
    // index — a different ShardIndex entry, hence the two passes.
    let mut batches: Vec<(Vec<u8>, Vec<Vec<u8>>)> = Vec::new();
    for si in &mut st.idx {
        if matches!(si.build, BuildState::Backfilling(_)) {
            st.stats_dirty = true;
        }
        advance_backfill(store, si, 2048);
        if let (Some(win), Some(dir), BuildState::Ready) = (&mut si.window, &segs_dir, &si.build) {
            // Exactly ONE windowed access path per table drives row
            // eviction (two drivers would seal the same batch twice);
            // every other windowed path only slides its own tree.
            let drives = window_driver(&ctx.state.catalogs, si.spec.name());
            if drives && let Some(rows) = win.pending_rows(&si.seg) {
                batches.push((table_of(si.spec.name()).to_vec(), rows));
            }
            st.stats_dirty |= evict_and_slide(win, si.spec.name(), &mut si.seg, store, dir, drives);
        }
    }
    // Pass 2: freeze each batch out of its table's text index.
    if let Some(dir) = &segs_dir {
        for (table, keys) in &batches {
            freeze_text_batches(st, table, keys, dir);
        }
    }
}

/// Σ approximate heap bytes of this shard's index segments, every
/// kind (scalar / text / ann / agg) — the tier's `reserved_bytes`
/// floor feed. Called per shard tick, gated on
/// tiering being enabled; refreshes the shard list first so a
/// just-declared index counts immediately.
/// FLUSHALL/FLUSHDB emptied this shard's store: every segment resets
/// to its declared-empty shape (found stale by an audit — the
/// embedded face's `on_commit` reset on FLUSH; this face kept serving
/// deleted keys out of IDX.QUERY). A mid-backfill index goes straight
/// to Ready: the keys it had left to walk no longer exist.
pub(crate) fn on_flush(ctx: &Ctx<'_>) {
    let mut st = ctx.shard.indexes.borrow_mut();
    refresh(ctx, &mut st);
    reset_all(&mut st);
}

/// Every segment back to its declared-empty shape, every build done.
fn reset_all(st: &mut ShardIndexes) {
    for si in &mut st.idx {
        si.seg = new_scalar_seg(&si.spec);
        si.text = new_text_seg(&si.spec);
        si.ann = new_ann_seg(&si.spec);
        si.agg = (si.spec.kind() == kevy_index::IndexKind::Agg).then(kevy_index::AggSegment::new);
        if let Some(g) = &mut si.global {
            g.clear(&si.spec);
        }
        si.build = BuildState::Ready;
        st.stats_dirty = true;
    }
}

/// Served from the generation cache: an idle store recomputes
/// nothing.
pub(crate) fn reserved_bytes(ctx: &Ctx<'_>) -> u64 {
    let mut st = ctx.shard.indexes.borrow_mut();
    refresh(ctx, &mut st);
    if !st.stats_dirty {
        return st.reserved_cache;
    }
    let sum = st
        .idx
        .iter()
        .map(|si| {
            si.entries().stats().approx_bytes
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
    name: &[u8],
    f: impl FnOnce(&IndexSpec, &Segment, Option<&kevy_window::WindowRt>) -> R,
) -> Result<R, CmdError> {
    let mut st = ctx.shard.indexes.borrow_mut();
    refresh(ctx, &mut st);
    let si = st.idx.iter().find(|si| si.spec.name() == name).ok_or("ERR no such index")?;
    // an owner of a global index answers once every shard has sent its
    // rows' entries (its own among them), whatever its own backfill's state
    let building = Err(CmdError::Wire("INDEXBUILDING index is still building"));
    match (&si.global, &si.build) {
        (Some(g), _) if !g.ready() => building,
        (Some(_), _) | (None, BuildState::Ready) => {
            Ok(f(&si.spec, si.entries(), si.window.as_ref()))
        }
        (None, BuildState::Backfilling(_)) => building,
        (None, BuildState::FailedOverBudget) => {
            Err(CmdError::Wire("INDEXOVERBUDGET index build exceeded MAXMEM"))
        }
    }
}

/// Run `f` against a READY aggregate segment.
pub(crate) fn with_ready_agg<R>(
    ctx: &Ctx<'_>,
    name: &[u8],
    f: impl FnOnce(&kevy_index::AggSegment) -> R,
) -> Result<R, CmdError> {
    let mut st = ctx.shard.indexes.borrow_mut();
    refresh(ctx, &mut st);
    let si = st.idx.iter().find(|si| si.spec.name() == name).ok_or("ERR no such index")?;
    match (&si.build, &si.agg) {
        (BuildState::Ready, Some(a)) => Ok(f(a)),
        (BuildState::Backfilling(_), _) => {
            Err(CmdError::Wire("INDEXBUILDING index is still building"))
        }
        (BuildState::FailedOverBudget, _) => {
            Err(CmdError::Wire("INDEXOVERBUDGET index build exceeded MAXMEM"))
        }
        (_, None) => Err(CmdError::Wire("ERR not an aggregate index")),
    }
}

/// Run `f` against a READY ANN graph (mutable for REBUILD).
pub(crate) fn with_ready_ann<R>(
    ctx: &Ctx<'_>,
    name: &[u8],
    f: impl FnOnce(&mut kevy_vector::Hnsw) -> R,
) -> Result<R, CmdError> {
    let mut st = ctx.shard.indexes.borrow_mut();
    refresh(ctx, &mut st);
    let si = st.idx.iter_mut().find(|si| si.spec.name() == name).ok_or("ERR no such index")?;
    match (&si.build, &mut si.ann) {
        (BuildState::Ready, Some(g)) => Ok(f(g)),
        (BuildState::Backfilling(_), _) => {
            Err(CmdError::Wire("INDEXBUILDING index is still building"))
        }
        (BuildState::FailedOverBudget, _) => {
            Err(CmdError::Wire("INDEXOVERBUDGET index build exceeded MAXMEM"))
        }
        (_, None) => Err(CmdError::Wire("ERR not a vector index")),
    }
}

/// Run `f` against a READY text segment.
pub(crate) fn with_ready_text_segment<R>(
    ctx: &Ctx<'_>,
    store: &mut Store,
    name: &[u8],
    f: impl FnOnce(
        &mut Store,
        &kevy_text::TextSegment,
        &kevy_index::IndexSpec,
        Option<&TextColdDir>,
    ) -> R,
) -> Result<R, CmdError> {
    let mut st = ctx.shard.indexes.borrow_mut();
    refresh(ctx, &mut st);
    let si = st.idx.iter().find(|si| si.spec.name() == name).ok_or("ERR no such index")?;
    match (&si.build, &si.text) {
        (BuildState::Ready, Some(ts)) => Ok(f(store, ts, &si.spec, si.cold_text.as_ref())),
        (BuildState::Backfilling(_), _) => {
            Err(CmdError::Wire("INDEXBUILDING index is still building"))
        }
        (BuildState::FailedOverBudget, _) => {
            Err(CmdError::Wire("INDEXOVERBUDGET index build exceeded MAXMEM"))
        }
        (_, None) => Err(CmdError::Wire("ERR not a text index")),
    }
}

/// Run `f` with a name→segment resolver over this shard's READY
/// segments (views probe several indexes per call). Building/failed
/// segments resolve to None.
pub(crate) fn with_segment_resolver<R>(
    ctx: &Ctx<'_>,
    f: impl for<'s> FnOnce(&'s dyn Fn(&[u8]) -> Option<&'s Segment>) -> R,
) -> R {
    let mut st = ctx.shard.indexes.borrow_mut();
    refresh(ctx, &mut st);
    let idx = &st.idx;
    // a view probes the rows of this shard, which a global index's
    // entries are not: it resolves to nothing, and the view refuses it
    let resolver = |name: &[u8]| -> Option<&Segment> {
        idx.iter()
            .find(|si| si.spec.name() == name && matches!(si.build, BuildState::Ready))
            .filter(|si| si.global.is_none())
            .map(|si| &si.seg)
    };
    f(&resolver)
}

/// Two-segment variant for COMPOSE — one RefCell borrow (nesting
/// [`with_ready_segment`] would double-borrow the shard's index list).
pub(crate) fn with_two_ready_segments<R>(
    ctx: &Ctx<'_>,
    a: &[u8],
    b: &[u8],
    f: impl FnOnce(&IndexSpec, &Segment, &IndexSpec, &Segment) -> R,
) -> Result<R, CmdError> {
    let mut st = ctx.shard.indexes.borrow_mut();
    refresh(ctx, &mut st);
    let ia = st.idx.iter().position(|si| si.spec.name() == a).ok_or("ERR no such index")?;
    let ib = st.idx.iter().position(|si| si.spec.name() == b).ok_or("ERR no such index")?;
    for i in [ia, ib] {
        if matches!(st.idx[i].build, BuildState::Backfilling(_)) {
            return Err(CmdError::Wire("INDEXBUILDING index is still building"));
        }
        // intersecting per shard needs both entries of a row on the row's shard
        if st.idx[i].global.is_some() {
            return Err(CmdError::Wire("ERR COMPOSE does not read a global index"));
        }
    }
    let (sa, sb) = (&st.idx[ia], &st.idx[ib]);
    Ok(f(&sa.spec, &sa.seg, &sb.spec, &sb.seg))
}

/// Whether this shard's slice of `name` is still backfilling.
pub(crate) fn segment_building(ctx: &Ctx<'_>, name: &[u8]) -> bool {
    let mut st = ctx.shard.indexes.borrow_mut();
    refresh(ctx, &mut st);
    st.idx.iter().find(|si| si.spec.name() == name).is_some_and(|si| match &si.global {
        // an owner waits for every shard; its own rows are one of them
        Some(g) => !g.ready(),
        None => matches!(si.build, BuildState::Backfilling(_)),
    })
}

/// Reconcile this shard's segment list with the shared catalog:
/// keep segments whose spec is unchanged, start backfills for new
/// ones, drop removed ones.
fn refresh(ctx: &Ctx<'_>, st: &mut ShardIndexes) {
    let catalogs = &ctx.state.catalogs;
    let (shard, n) = (ctx.shard.shard_id(), ctx.state.nshards());
    let generation = catalogs.index_gen();
    if st.generation == generation {
        set_key_dirs(catalogs, st);
        return;
    }
    st.stats_dirty = true;
    let cat = catalogs.index();
    let mut next: Vec<ShardIndex> = Vec::new();
    if let Some(cat) = cat {
        for (spec, _state) in cat.iter() {
            let part = cat.partitioning(spec.name());
            let inc = catalogs.incarnation(spec.name());
            let same = |si: &ShardIndex| {
                si.spec == *spec
                    && si.global.as_ref().map_or(!part.is_global(), |g| g.fits((shard, n), inc))
            };
            match st.idx.iter().position(same) {
                Some(i) => {
                    let mut si = st.idx.swap_remove(i);
                    // The index spec survived, but the table's WINDOW
                    // clause may have changed (REPLACE): reconcile.
                    // A changed window resets the runtime — the old
                    // spill is unreachable and swept on first slide.
                    let want = window_for(catalogs, &si.spec);
                    let have = si.window.as_ref().map(|w| (w.spec().clone(), w.shape()));
                    if have != want {
                        si.window = want.map(|(w, sh)| kevy_window::WindowRt::new(w, sh));
                    }
                    let want_text = text_window_for(catalogs, &si.spec);
                    if si.cold_text.is_some() != want_text {
                        si.cold_text = want_text.then(TextColdDir::new);
                    }
                    next.push(si);
                }
                None => {
                    let mut si = fresh_shard_index(catalogs, spec);
                    si.global = part
                        .is_global()
                        .then(|| global::GlobalRole::new(spec, part, (shard, n), inc));
                    next.push(si);
                }
            }
        }
    }
    st.any_global = next.iter().any(|si| si.global.is_some());
    st.idx = next;
    st.generation = generation;
    st.view_gen = u64::MAX;
    set_key_dirs(catalogs, st);
}

/// Give a key directory to exactly the indexes some view reads by key.
fn set_key_dirs(catalogs: &CatalogState, st: &mut ShardIndexes) {
    let view_gen = catalogs.view_gen();
    if st.view_gen == view_gen {
        return;
    }
    let mut read: Vec<Vec<u8>> = Vec::new();
    if let Some(cat) = catalogs.view() {
        for spec in cat.iter() {
            read.push(spec.order_by.clone());
            spec.tree.each_leaf(&mut |l| read.push(l.index.clone()));
        }
    }
    for si in &mut st.idx {
        si.seg.set_key_dir(read.iter().any(|n| n.as_slice() == si.spec.name()));
    }
    st.view_gen = view_gen;
}

impl ShardIndex {
    /// Whether this index finds a row's old entry from the store's record
    /// (a scalar one); text, vector and aggregate indexes keep their own
    /// map from row to entry.
    fn reads_old_rows(&self) -> bool {
        self.text.is_none() && self.ann.is_none() && self.agg.is_none()
    }

    /// The entries this shard answers for: its own rows' for a local
    /// index, the partition it owns for a global one (none when it owns
    /// none — P ≤ N, and each partition has its own shard).
    fn entries(&self) -> &Segment {
        match &self.global {
            Some(g) => g.owned.first().map_or(&self.seg, |(_, seg)| seg),
            None => &self.seg,
        }
    }
}

/// A just-declared index's runtime entry. Its backfill walks the
/// domain's keys on THIS shard; live writes from now on hit the hook
/// first and win.
fn fresh_shard_index(catalogs: &CatalogState, spec: &IndexSpec) -> ShardIndex {
    ShardIndex {
        agg: (spec.kind() == kevy_index::IndexKind::Agg).then(kevy_index::AggSegment::new),
        text: new_text_seg(spec),
        ann: new_ann_seg(spec),
        seg: new_scalar_seg(spec),
        window: window_for(catalogs, spec).map(|(w, sh)| kevy_window::WindowRt::new(w, sh)),
        cold_text: text_window_for(catalogs, spec).then(TextColdDir::new),
        global: None,
        spec: spec.clone(),
        build: BuildState::Backfilling(KeyWalk::new(spec.prefix())),
        slots: None,
    }
}

pub(crate) use kevy_window::{ColdHit, ColdPageQuery, TextColdDir, WindowRt};
mod window_slide;
use window_slide::{
    evict_and_slide, freeze_text_batches, shard_segs_dir, table_of, text_window_for, window_driver,
    window_for,
};
mod global_verify;
pub(crate) use global_verify::{Placed, VERIFY_TAG, verify_chunk as global_verify_chunk};
mod global;
pub(crate) use global::{apply_ext, take_ext_out};
mod quantile;
pub(crate) use quantile::{POINTS_PER_PARTITION, put_points, quantile_points, read_points};
mod global_wire;
mod row_apply;
use row_apply::advance_backfill;
pub(crate) use row_apply::{RowValue, row_value};
mod changes;
mod seg_new;
use seg_new::{new_ann_seg, new_scalar_seg, new_text_seg};

#[cfg(test)]
mod tests;
