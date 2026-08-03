//! The view engine's runtime half (mirrors
//! [`crate::index_runtime`]): a process-wide [`kevy_index::ViewCatalog`]
//! behind RwLock + generation, both owned by `RuntimeState.catalogs`;
//! per-shard view states in `ShardCtx.views`, and the
//! write-hook maintenance that runs AFTER index maintenance (so the
//! membership probes see fresh segments). Callers gate both hooks on
//! the `VIEW_NONEMPTY` gate bit.

use kevy_resp::CmdError;
use kevy_index::{IndexValue, MaterializedSet, ViewMode, ViewSpec, eval_tree, key_in_tree};
use kevy_store::Store;

use crate::state::{CatalogState, Ctx, ShardCtx};

struct ViewState {
    spec: ViewSpec,
    /// `Some` for materialized views.
    mat: Option<MaterializedSet>,
    /// Local rebuild scheduled (initial build / top-K underflow).
    needs_rebuild: bool,
}

/// One shard's view states. Owned by `crate::state::ShardCtx`.
#[derive(Default)]
pub(crate) struct ShardViews {
    generation: u64,
    views: Vec<ViewState>,
    /// `reserved_bytes` generation cache — see
    /// `ShardIndexes::stats_dirty`; same contract, view half.
    stats_dirty: bool,
    reserved_cache: u64,
    /// Union of every view-referenced index name (order + leaves) —
    /// the write hook probes each exactly once per key.
    referenced: Vec<Vec<u8>>,
}

/// Write hook — call AFTER `index_runtime::on_write` so segment
/// probes see the fresh row.
#[inline]
pub(crate) fn on_write(ctx: &Ctx<'_>, store: &mut Store, key: &[u8]) {
    let mut st = ctx.shard.views.borrow_mut();
    refresh(&ctx.state.catalogs, &mut st);
    // Probe each referenced index ONCE for this key, then evaluate
    // every view against the same value table (bounds compares
    // only — no per-view re-hashing; this took the measured write
    // tax from 44% to the clamp band).
    let st = &mut *st;
    let referenced = &st.referenced;
    crate::index_runtime::with_segment_resolver(ctx, store, |seg| {
        let vals: Vec<(&[u8], Option<kevy_index::IndexValue>)> = referenced
            .iter()
            .map(|n| {
                (
                    n.as_slice(),
                    seg(n).and_then(|s| s.verify_entry(key)).cloned(),
                )
            })
            .collect();
        let lookup = |name: &[u8]| -> Option<kevy_index::IndexValue> {
            vals.iter().find(|(n, _)| *n == name).and_then(|(_, v)| v.clone())
        };
        for vs in &mut st.views {
            let Some(mat) = &mut vs.mat else { continue };
            st.stats_dirty = true;
            let member = kevy_index::key_in_tree_vals(&vs.spec.tree, &lookup);
            let order = lookup(&vs.spec.order_by);
            if mat.apply(key, member, order) {
                vs.needs_rebuild = true;
            }
        }
    });
}

/// Tick hook — run scheduled local rebuilds.
pub(crate) fn on_tick(ctx: &Ctx<'_>, store: &mut Store) {
    let mut st = ctx.shard.views.borrow_mut();
    refresh(&ctx.state.catalogs, &mut st);
    let st = &mut *st;
    for vs in &mut st.views {
        if vs.needs_rebuild {
            rebuild_local(ctx, store, vs);
            st.stats_dirty = true;
        }
    }
}

/// Σ approximate heap bytes of this shard's materialized view sets —
/// the view half of the tier's `reserved_bytes` floor feed.
/// Virtual views hold no set, so they contribute nothing.
/// FLUSHALL/FLUSHDB emptied the keyspace: every materialized set
/// clears with it (twin of `index_runtime::on_flush`).
pub(crate) fn on_flush(ctx: &Ctx<'_>) {
    let mut st = ctx.shard.views.borrow_mut();
    refresh(&ctx.state.catalogs, &mut st);
    let st = &mut *st;
    for vs in &mut st.views {
        if let Some(m) = &mut vs.mat {
            m.clear();
            vs.needs_rebuild = false;
            st.stats_dirty = true;
        }
    }
}

/// Served from the generation cache.
pub(crate) fn reserved_bytes(ctx: &Ctx<'_>) -> u64 {
    let mut st = ctx.shard.views.borrow_mut();
    refresh(&ctx.state.catalogs, &mut st);
    if !st.stats_dirty {
        return st.reserved_cache;
    }
    let sum = st
        .views
        .iter()
        .map(|vs| vs.mat.as_ref().map_or(0, kevy_index::MaterializedSet::approx_bytes))
        .sum();
    st.reserved_cache = sum;
    st.stats_dirty = false;
    sum
}

/// Query access to one view's per-shard answer. For virtual views the
/// tree evaluates now; materialized views page their set. Returns
/// `(order, key)` ascending (the reduce applies DESC).
pub(crate) fn shard_page(
    ctx: &Ctx<'_>,
    store: &mut Store,
    name: &[u8],
    after: Option<&(IndexValue, Vec<u8>)>,
    limit: usize,
) -> Result<Vec<(IndexValue, Vec<u8>)>, CmdError> {
    let mut st = ctx.shard.views.borrow_mut();
    refresh(&ctx.state.catalogs, &mut st);
    let st = &mut *st;
    let vs = st
        .views
        .iter_mut()
        .find(|v| v.spec.name == name)
        .ok_or("ERR no such view")?;
    if referenced_index_building(ctx, store, &vs.spec) {
        return Err(CmdError::Wire("INDEXBUILDING view's base index is still building"));
    }
    if vs.needs_rebuild {
        rebuild_local(ctx, store, vs);
        st.stats_dirty = true;
    }
    let desc = vs.spec.desc;
    match &vs.mat {
        Some(m) => Ok(m.page(after, limit, desc)),
        None => {
            // Virtual: stream the ORDER index in order and probe
            // membership per candidate — O(limit / selectivity)
            // probes instead of materializing the member set
            // (which measured 9.6ms p99 at 1M×2 components; the
            // RFC clamp is 3ms).
            let spec = vs.spec.clone();
            Ok(virtual_page(ctx, store, &spec, after, limit, desc))
        }
    }
}

/// Per-shard stats for LIST/VERIFY: (members, bytes, order_excluded,
/// building) — virtual views report a fresh evaluation's cardinality.
pub(crate) fn shard_stats(
    ctx: &Ctx<'_>,
    store: &mut Store,
    name: &[u8],
) -> Result<(u64, u64, u64, bool), CmdError> {
    let mut st = ctx.shard.views.borrow_mut();
    refresh(&ctx.state.catalogs, &mut st);
    let st = &mut *st;
    let vs = st
        .views
        .iter_mut()
        .find(|v| v.spec.name == name)
        .ok_or("ERR no such view")?;
    match &vs.mat {
        Some(m) => Ok((m.len() as u64, m.approx_bytes(), m.order_excluded, vs.needs_rebuild)),
        None => {
            let spec = vs.spec.clone();
            let n = eval_with_order(ctx, store, &spec).len() as u64;
            Ok((n, 0, 0, false))
        }
    }
}

/// Force a local rebuild (VIEW.REBUILD).
pub(crate) fn schedule_rebuild(shard: &ShardCtx, name: &[u8]) {
    let mut st = shard.views.borrow_mut();
    for vs in &mut st.views {
        if vs.spec.name == name {
            vs.needs_rebuild = true;
        }
    }
}

fn refresh(catalogs: &CatalogState, st: &mut ShardViews) {
    let generation = catalogs.view_gen();
    if st.generation == generation {
        return;
    }
    st.stats_dirty = true;
    let cat = catalogs.view();
    let mut next = Vec::new();
    if let Some(cat) = cat {
        for spec in cat.iter() {
            match st.views.iter().position(|v| v.spec == *spec) {
                Some(i) => next.push(st.views.swap_remove(i)),
                None => {
                    let mat = match spec.mode {
                        ViewMode::Virtual => None,
                        ViewMode::Materialized { top_k } => Some(MaterializedSet::new(top_k, spec.desc)),
                    };
                    next.push(ViewState {
                        spec: spec.clone(),
                        needs_rebuild: mat.is_some(),
                        mat,
                    });
                }
            }
        }
    }
    st.views = next;
    st.generation = generation;
    let mut referenced: Vec<Vec<u8>> = Vec::new();
    for vs in &st.views {
        if vs.mat.is_none() {
            continue; // virtual views don't ride the write hook
        }
        let mut add = |n: &[u8]| {
            if !referenced.iter().any(|r| r == n) {
                referenced.push(n.to_vec());
            }
        };
        add(&vs.spec.order_by);
        vs.spec.tree.each_leaf(&mut |l| add(&l.index));
    }
    st.referenced = referenced;
}

/// Order-driven virtual page (see the call site).
fn virtual_page(
    ctx: &Ctx<'_>,
    store: &mut Store,
    spec: &ViewSpec,
    after: Option<&(IndexValue, Vec<u8>)>,
    limit: usize,
    desc: bool,
) -> Vec<(IndexValue, Vec<u8>)> {
    crate::index_runtime::with_segment_resolver(ctx, store, |seg| {
        let Some(order_seg) = seg(&spec.order_by) else {
            return Vec::new();
        };
        let cursor = after.map(|(v, k)| kevy_index::Cursor { value: v.clone(), key: k.clone() });
        let mut out = Vec::with_capacity(limit.min(256));
        for (v, k) in order_seg.scan(cursor.as_ref(), desc) {
            if key_in_tree(&spec.tree, k, &seg) {
                out.push((v.clone(), k.to_vec()));
                if out.len() == limit {
                    break;
                }
            }
        }
        out
    })
}

/// Evaluate membership + order for every member on this shard.
fn eval_with_order(
    ctx: &Ctx<'_>,
    store: &mut Store,
    spec: &ViewSpec,
) -> Vec<(IndexValue, Vec<u8>)> {
    crate::index_runtime::with_segment_resolver(ctx, store, |seg| {
        let members = eval_tree(&spec.tree, &seg);
        members
            .into_iter()
            .filter_map(|k| {
                seg(&spec.order_by)
                    .and_then(|s| s.verify_entry(&k))
                    .map(|v| (v.clone(), k))
            })
            .collect()
    })
}

fn rebuild_local(ctx: &Ctx<'_>, store: &mut Store, vs: &mut ViewState) {
    let spec = vs.spec.clone();
    let Some(mat) = &mut vs.mat else {
        vs.needs_rebuild = false;
        return;
    };
    mat.clear();
    let mut rows = eval_with_order(ctx, store, &spec);
    rows.sort();
    if let ViewMode::Materialized { top_k } = spec.mode
        && top_k > 0
    {
        rows.truncate((top_k + top_k / 4) as usize);
    }
    for (v, k) in rows {
        mat.apply(&k, true, Some(v));
    }
    vs.needs_rebuild = false;
}

/// A view is unanswerable while ANY referenced index (leaves + the
/// order index) is still backfilling — the resolver hides Building
/// segments, and an empty leaf would silently misreport membership.
fn referenced_index_building(ctx: &Ctx<'_>, store: &mut Store, spec: &ViewSpec) -> bool {
    let mut names: Vec<Vec<u8>> = vec![spec.order_by.clone()];
    spec.tree.each_leaf(&mut |l| names.push(l.index.clone()));
    names
        .iter()
        .any(|n| crate::index_runtime::segment_building(ctx, store, n))
}
