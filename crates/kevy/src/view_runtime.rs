//! v2.6 — the view engine's runtime half (mirrors
//! [`crate::index_runtime`]): a process-global [`ViewCatalog`] behind
//! RwLock + generation, per-shard view states in `ShardCtx.views`,
//! and the write-hook maintenance that runs AFTER index maintenance
//! (so the membership probes see fresh segments). Callers gate both
//! hooks on the `VIEW_NONEMPTY` gate bit (K-103 W5).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use kevy_index::{IndexValue, MaterializedSet, ViewCatalog, ViewMode, ViewSpec, eval_tree, key_in_tree};
use kevy_store::Store;

use crate::state::ShardCtx;

static GEN: AtomicU64 = AtomicU64::new(0);
static CATALOG: RwLock<Option<Arc<ViewCatalog>>> = RwLock::new(None);

struct ViewState {
    spec: ViewSpec,
    /// `Some` for materialized views.
    mat: Option<MaterializedSet>,
    /// Local rebuild scheduled (initial build / top-K underflow).
    needs_rebuild: bool,
}

/// One shard's view states. Owned by `crate::state::ShardCtx` (W4).
#[derive(Default)]
pub(crate) struct ShardViews {
    generation: u64,
    views: Vec<ViewState>,
    /// Union of every view-referenced index name (order + leaves) —
    /// the write hook probes each exactly once per key.
    referenced: Vec<Vec<u8>>,
}

/// Current catalog snapshot.
pub(crate) fn catalog() -> Option<Arc<ViewCatalog>> {
    CATALOG.read().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
}

/// Is at least one view declared? Cold-path input to the per-shard
/// `VIEW_NONEMPTY` gate bit.
pub(crate) fn catalog_nonempty() -> bool {
    CATALOG
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .is_some_and(|c| !c.is_empty())
}

/// Swap in a new catalog version, then bump the control epoch (writer
/// protocol step ② — see `index_runtime::install_catalog`).
pub(crate) fn install_catalog(control_epoch: &AtomicU64, c: ViewCatalog) {
    *CATALOG.write().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(c));
    GEN.fetch_add(1, Ordering::Release);
    control_epoch.fetch_add(1, Ordering::Release);
}

/// Write hook — call AFTER `index_runtime::on_write` so segment
/// probes see the fresh row.
#[inline]
pub(crate) fn on_write(shard: &ShardCtx, store: &mut Store, key: &[u8]) {
    let mut st = shard.views.borrow_mut();
    refresh(&mut st);
    // Probe each referenced index ONCE for this key, then evaluate
    // every view against the same value table (bounds compares
    // only — no per-view re-hashing; this took the measured write
    // tax from 44% to the clamp band).
    let st = &mut *st;
    let referenced = &st.referenced;
    crate::index_runtime::with_segment_resolver(shard, store, |seg| {
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
            let member = kevy_index::key_in_tree_vals(&vs.spec.tree, &lookup);
            let order = lookup(&vs.spec.order_by);
            if mat.apply(key, member, order) {
                vs.needs_rebuild = true;
            }
        }
    });
}

/// Tick hook — run scheduled local rebuilds.
pub(crate) fn on_tick(shard: &ShardCtx, store: &mut Store) {
    let mut st = shard.views.borrow_mut();
    refresh(&mut st);
    for vs in &mut st.views {
        if vs.needs_rebuild {
            rebuild_local(shard, store, vs);
        }
    }
}

/// Query access to one view's per-shard answer. For virtual views the
/// tree evaluates now; materialized views page their set. Returns
/// `(order, key)` ascending (the reduce applies DESC).
pub(crate) fn shard_page(
    shard: &ShardCtx,
    store: &mut Store,
    name: &[u8],
    after: Option<&(IndexValue, Vec<u8>)>,
    limit: usize,
) -> Result<Vec<(IndexValue, Vec<u8>)>, &'static str> {
    let mut st = shard.views.borrow_mut();
    refresh(&mut st);
    let vs = st
        .views
        .iter_mut()
        .find(|v| v.spec.name == name)
        .ok_or("ERR no such view")?;
    if referenced_index_building(shard, store, &vs.spec) {
        return Err("INDEXBUILDING view's base index is still building");
    }
    if vs.needs_rebuild {
        rebuild_local(shard, store, vs);
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
            Ok(virtual_page(shard, store, &spec, after, limit, desc))
        }
    }
}

/// Per-shard stats for LIST/VERIFY: (members, bytes, order_excluded,
/// building) — virtual views report a fresh evaluation's cardinality.
pub(crate) fn shard_stats(
    shard: &ShardCtx,
    store: &mut Store,
    name: &[u8],
) -> Result<(u64, u64, u64, bool), &'static str> {
    let mut st = shard.views.borrow_mut();
    refresh(&mut st);
    let vs = st
        .views
        .iter_mut()
        .find(|v| v.spec.name == name)
        .ok_or("ERR no such view")?;
    match &vs.mat {
        Some(m) => Ok((m.len() as u64, m.approx_bytes(), m.order_excluded, vs.needs_rebuild)),
        None => {
            let spec = vs.spec.clone();
            let n = eval_with_order(shard, store, &spec).len() as u64;
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

fn refresh(st: &mut ShardViews) {
    let generation = GEN.load(Ordering::Acquire);
    if st.generation == generation {
        return;
    }
    let cat = catalog();
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
    shard: &ShardCtx,
    store: &mut Store,
    spec: &ViewSpec,
    after: Option<&(IndexValue, Vec<u8>)>,
    limit: usize,
    desc: bool,
) -> Vec<(IndexValue, Vec<u8>)> {
    crate::index_runtime::with_segment_resolver(shard, store, |seg| {
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
    shard: &ShardCtx,
    store: &mut Store,
    spec: &ViewSpec,
) -> Vec<(IndexValue, Vec<u8>)> {
    crate::index_runtime::with_segment_resolver(shard, store, |seg| {
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

fn rebuild_local(shard: &ShardCtx, store: &mut Store, vs: &mut ViewState) {
    let spec = vs.spec.clone();
    let Some(mat) = &mut vs.mat else {
        vs.needs_rebuild = false;
        return;
    };
    mat.clear();
    let mut rows = eval_with_order(shard, store, &spec);
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
fn referenced_index_building(shard: &ShardCtx, store: &mut Store, spec: &ViewSpec) -> bool {
    let mut names: Vec<Vec<u8>> = vec![spec.order_by.clone()];
    spec.tree.each_leaf(&mut |l| names.push(l.index.clone()));
    names
        .iter()
        .any(|n| crate::index_runtime::segment_building(shard, store, n))
}
