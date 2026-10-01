//! Embedded views (server parity minus VIA/FIELDS hydration:
//! in-process callers dereference and read fields directly).
//!
//! Mirrors the embedded index architecture: per-shard view states in
//! `Inner` (maintained inside `commit_write` right after index
//! maintenance, under the same shard lock), a store-level registry,
//! synchronous builds, typed API (`Tree` passed directly — no text
//! grammar in-process).

use crate::{KevyError, KevyResult};
use std::io;
use std::sync::RwLock;

use kevy_index::{
    IndexValue, MaterializedSet, Membership, SortOrder, Tree, ViewCatalog, ViewMode, ViewSpec,
};

use crate::ops_index::ShardSegs;
use crate::store::{Store, lock_write};

/// Store-level registry.
#[derive(Debug, Default)]
pub(crate) struct ViewReg {
    pub(crate) catalog: RwLock<(u64, ViewCatalog)>,
}

/// One shard's view states (inside `Inner`, guarded by the shard lock).
#[derive(Debug, Default)]
pub(crate) struct ShardViews {
    pub(crate) version: u64,
    /// The (view catalog, index list) versions the key directories were
    /// last set for.
    pub(crate) dirs_at: (u64, u64),
    pub(crate) views: Vec<ViewState>,
    /// `reserved_bytes` generation cache — see
    /// `ShardSegs::stats_dirty`; same contract, view half. Tier-only,
    /// like its twin.
    #[cfg(all(feature = "tier", not(target_arch = "wasm32")))]
    pub(crate) stats_dirty: bool,
    #[cfg(all(feature = "tier", not(target_arch = "wasm32")))]
    pub(crate) reserved_cache: u64,
}

#[derive(Debug)]

pub(crate) struct ViewState {
    spec: ViewSpec,
    mat: Option<MaterializedSet>,
    needs_rebuild: bool,
}

impl ShardViews {
    /// Invalidate the cache — no-op without the tier backend; see
    /// `ShardSegs::mark_stats_dirty`.
    #[inline]
    pub(crate) fn mark_stats_dirty(&mut self) {
        #[cfg(all(feature = "tier", not(target_arch = "wasm32")))]
        {
            self.stats_dirty = true;
        }
    }

    /// Σ approximate heap bytes of the materialized view sets — the
    /// view half of the tier's `reserved_bytes` feed.
    /// Virtual views hold no set and contribute nothing.
    #[cfg(all(feature = "tier", not(target_arch = "wasm32")))]
    pub(crate) fn reserved_bytes(&mut self) -> u64 {
        if !self.stats_dirty {
            return self.reserved_cache;
        }
        let sum = self
            .views
            .iter()
            .map(|v| v.mat.as_ref().map_or(0, MaterializedSet::approx_bytes))
            .sum();
        self.reserved_cache = sum;
        self.stats_dirty = false;
        sum
    }
}

/// One page of view members plus the resume cursor.
///
/// ```
/// use kevy_embedded::*;
/// let s = Store::open(Config::default())?;
/// s.idx_create(b"by_pri", b"t:", b"pri", IndexValType::I64, IndexKind::Range)?;
/// for (k, pri) in [(&b"t:1"[..], &b"5"[..]), (b"t:2", b"9")] {
///     s.hset(k, &[(b"pri", pri)])?;
/// }
/// let all = ViewTree::Leaf(ViewLeaf::new(b"by_pri".to_vec(), IndexValue::I64(0), IndexValue::I64(99)));
/// s.view_create(b"urgent", all, b"by_pri", SortOrder::Desc, ViewMode::Virtual)?;
/// let (members, after): ViewPage = s.view_query(b"urgent", None, 1)?;
/// assert_eq!(members, [(b"t:2".to_vec(), IndexValue::I64(9))]);
/// let (rest, _) = s.view_query(b"urgent", after.as_ref(), 10)?; // resume past it
/// assert_eq!(rest[0].0, b"t:1");
/// # Ok::<(), kevy_embedded::KevyError>(())
/// ```
pub type ViewPage = (Vec<(Vec<u8>, IndexValue)>, Option<(IndexValue, Vec<u8>)>);

impl Store {
    /// Declare a view (typed tree; `via` is not supported embedded —
    /// read fields in-process). Builds synchronously.
    pub fn view_create(
        &self,
        name: &[u8],
        tree: Tree,
        order_by: &[u8],
        order: SortOrder,
        mode: ViewMode,
    ) -> KevyResult<()> {
        self.catalog_change(|| self.create_view(name, tree, order_by, order, mode))
    }

    fn create_view(
        &self,
        name: &[u8],
        tree: Tree,
        order_by: &[u8],
        order: SortOrder,
        mode: ViewMode,
    ) -> KevyResult<()> {
        self.check_view_refs(&tree, order_by)?;
        let spec = ViewSpec::new(name, tree, order_by).with_order(order).with_mode(mode);
        {
            let mut g =
                self.views.catalog.write().unwrap_or_else(std::sync::PoisonError::into_inner);
            let (ver, cat) = &mut *g;
            cat.create(spec).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
            *ver += 1;
        }
        for shard in self.shards.iter() {
            let mut g = lock_write(shard);
            let inner = &mut *g;
            crate::ops_index::sync_segs(&self.indexes, &mut inner.idx_segs, &mut inner.store);
            sync_views(&self.views, &mut inner.view_segs, &mut inner.idx_segs);
        }
        Ok(())
    }

    /// Every index a view references (its leaves + ORDER BY) must
    /// already be declared.
    fn check_view_refs(&self, tree: &Tree, order_by: &[u8]) -> KevyResult<()> {
        let mut names: Vec<Vec<u8>> = vec![order_by.to_vec()];
        tree.each_leaf(&mut |l| names.push(l.index.clone()));
        let g = self.indexes.catalog.read().unwrap_or_else(std::sync::PoisonError::into_inner);
        for n in &names {
            if g.1.get(n).is_none() {
                return Err(KevyError::InvalidInput("view references unknown index".into()));
            }
        }
        Ok(())
    }

    /// Drop a view; `false` if absent. Refused on a replica and after
    /// [`Store::shutdown`], like every write.
    pub fn view_drop(&self, name: &[u8]) -> KevyResult<bool> {
        self.catalog_change(|| {
            let mut g =
                self.views.catalog.write().unwrap_or_else(std::sync::PoisonError::into_inner);
            let (ver, cat) = &mut *g;
            let hit = cat.drop_view(name);
            if hit {
                *ver += 1;
            }
            Ok(hit)
        })
    }

    /// Ordered page across shards (`after` resumes exclusively; DESC
    /// views page from the large end).
    pub fn view_query(
        &self,
        name: &[u8],
        after: Option<&(IndexValue, Vec<u8>)>,
        limit: usize,
    ) -> KevyResult<ViewPage> {
        let limit = limit.clamp(1, 100_000);
        let mut desc = false;
        let mut all: Vec<(IndexValue, Vec<u8>)> = Vec::new();
        let mut found = false;
        for shard in self.shards.iter() {
            let mut g = lock_write(shard);
            let inner = &mut *g;
            crate::ops_index::sync_segs(&self.indexes, &mut inner.idx_segs, &mut inner.store);
            sync_views(&self.views, &mut inner.view_segs, &mut inner.idx_segs);
            let Some(i) = inner.view_segs.views.iter().position(|v| v.spec.name == name) else {
                continue;
            };
            found = true;
            if inner.view_segs.views[i].needs_rebuild {
                inner.view_segs.mark_stats_dirty();
                rebuild(&mut inner.view_segs.views[i], &inner.idx_segs);
            }
            let vs = &inner.view_segs.views[i];
            desc = vs.spec.order == SortOrder::Desc;
            match &vs.mat {
                Some(m) => all.extend(m.page(after, limit)),
                None => stream_virtual(&vs.spec, &inner.idx_segs, after, limit, &mut all),
            }
        }
        if !found {
            return Err(KevyError::NotFound("no such view".into()));
        }
        all.sort();
        if desc {
            all.reverse();
        }
        all.truncate(limit);
        let next = if all.len() == limit { all.last().cloned() } else { None };
        Ok((all.into_iter().map(|(v, k)| (k, v)).collect(), next))
    }

    /// Declared views (name, mode, leaves).
    pub fn view_list(&self) -> Vec<(Vec<u8>, ViewMode, usize)> {
        let g = self.views.catalog.read().unwrap_or_else(std::sync::PoisonError::into_inner);
        g.1.iter().map(|s| (s.name.clone(), s.mode, s.tree.leaves())).collect()
    }

    /// The declaration of the view named `name`, as the catalog holds it.
    pub fn view_spec(&self, name: &[u8]) -> Option<kevy_index::ViewSpec> {
        let g = self.views.catalog.read().unwrap_or_else(std::sync::PoisonError::into_inner);
        g.1.get(name).cloned()
    }

    /// Summed member count across shards.
    pub fn view_count(&self, name: &[u8]) -> KevyResult<u64> {
        Ok(self.view_query(name, None, 100_000)?.0.len() as u64)
    }
}

fn resolver<'a>(segs: &'a ShardSegs) -> impl Fn(&[u8]) -> Option<&'a kevy_index::Segment> {
    move |name: &[u8]| segs.segs.iter().find(|(s, _)| s.name() == name).map(|(_, seg)| seg)
}

fn eval_shard(spec: &ViewSpec, segs: &ShardSegs) -> Vec<(IndexValue, Vec<u8>)> {
    let r = resolver(segs);
    let members = spec.tree.eval(&&r);
    members
        .into_iter()
        .filter_map(|k| r(&spec.order_by).and_then(|s| s.key_dir()?.get(&k)).map(|v| (v, k)))
        .collect()
}

/// Virtual-mode page: order-driven streaming over the ORDER BY index,
/// probing the tree per key (same clamp rationale as the server
/// pager).
fn stream_virtual(
    spec: &ViewSpec,
    segs: &ShardSegs,
    after: Option<&(IndexValue, Vec<u8>)>,
    limit: usize,
    all: &mut Vec<(IndexValue, Vec<u8>)>,
) {
    let r = resolver(segs);
    if let Some(order_seg) = r(&spec.order_by) {
        let cursor = after.map(|(v, k)| kevy_index::Cursor::new(v.clone(), k.clone()));
        let mut got = 0usize;
        let mut scan = order_seg.scan(cursor.as_ref(), spec.order);
        while let Some((v, k)) = scan.next_entry() {
            if spec.tree.contains(k, &&r) {
                all.push((v.clone(), k.to_vec()));
                got += 1;
                if got == limit {
                    break;
                }
            }
        }
    }
}

fn rebuild(vs: &mut ViewState, segs: &ShardSegs) {
    let spec = vs.spec.clone();
    let Some(mat) = &mut vs.mat else {
        vs.needs_rebuild = false;
        return;
    };
    mat.clear();
    let mut rows = eval_shard(&spec, segs);
    rows.sort();
    if let ViewMode::Materialized { top_k } = spec.mode
        && top_k > 0
    {
        if spec.order == SortOrder::Desc {
            rows.reverse();
        }
        rows.truncate((top_k + top_k / 4) as usize);
    }
    for (v, k) in rows {
        mat.apply(&k, Membership::Member(Some(v)));
    }
    vs.needs_rebuild = false;
}

/// Reconcile with the catalog (under the shard lock).
pub(crate) fn sync_views(reg: &ViewReg, sv: &mut ShardViews, segs: &mut ShardSegs) {
    let g = reg.catalog.read().unwrap_or_else(std::sync::PoisonError::into_inner);
    let (ver, cat) = &*g;
    // a view reads its indexes by key: exactly those keep a key directory
    if sv.dirs_at != (*ver, segs.version) {
        let mut read: Vec<Vec<u8>> = Vec::new();
        for v in cat.iter() {
            read.push(v.order_by.clone());
            v.tree.each_leaf(&mut |l| read.push(l.index.clone()));
        }
        for (spec, seg) in &mut segs.segs {
            seg.set_key_dir(read.iter().any(|n| n.as_slice() == spec.name()));
        }
        sv.dirs_at = (*ver, segs.version);
    }
    let segs = &*segs;
    if sv.version == *ver {
        return;
    }
    sv.mark_stats_dirty();
    let mut next = Vec::new();
    for spec in cat.iter() {
        match sv.views.iter().position(|v| v.spec == *spec) {
            Some(i) => next.push(sv.views.swap_remove(i)),
            None => {
                // only a materialized view keeps a set; every other mode reads at query time
                let mat = match spec.mode {
                    ViewMode::Materialized { top_k } => {
                        Some(MaterializedSet::new(top_k, spec.order))
                    }
                    _ => None,
                };
                let mut vs = ViewState { spec: spec.clone(), needs_rebuild: mat.is_some(), mat };
                if vs.needs_rebuild {
                    rebuild(&mut vs, segs);
                }
                next.push(vs);
            }
        }
    }
    sv.views = next;
    sv.version = *ver;
}

/// Write hook — call AFTER `ops_index::on_commit` (same shard lock).
pub(crate) fn on_commit(reg: &ViewReg, sv: &mut ShardViews, segs: &mut ShardSegs, parts: &[&[u8]]) {
    {
        let g = reg.catalog.read().unwrap_or_else(std::sync::PoisonError::into_inner);
        if g.1.is_empty() {
            return;
        }
    }
    sync_views(reg, sv, segs);
    let segs = &*segs;
    let verb = parts.first().copied().unwrap_or(b"");
    if verb.eq_ignore_ascii_case(b"FLUSHALL") || verb.eq_ignore_ascii_case(b"FLUSHDB") {
        for vs in &mut sv.views {
            if let Some(m) = &mut vs.mat {
                m.clear();
            }
        }
        sv.mark_stats_dirty();
        return;
    }
    // Same exact written-key walk as the index hook.
    let mut touched = false;
    let views = &mut sv.views;
    crate::ops_index::each_written_key_pub(verb, parts, |key| {
        for vs in &mut *views {
            let Some(mat) = &mut vs.mat else { continue };
            touched = true;
            let r = resolver(segs);
            let membership = if vs.spec.tree.contains(key, &&r) {
                Membership::Member(r(&vs.spec.order_by).and_then(|s| s.key_dir()?.get(key)))
            } else {
                Membership::NonMember
            };
            if mat.apply(key, membership) {
                vs.needs_rebuild = true;
            }
        }
    });
    if touched {
        sv.mark_stats_dirty();
    }
}
