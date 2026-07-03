//! v2.6 — embedded views (server parity minus VIA/FIELDS hydration:
//! in-process callers dereference and read fields directly).
//!
//! Mirrors the embedded index architecture: per-shard view states in
//! `Inner` (maintained inside `commit_write` right after index
//! maintenance, under the same shard lock), a store-level registry,
//! synchronous builds, typed API (`Tree` passed directly — no text
//! grammar in-process).

use std::io;
use std::sync::RwLock;

use kevy_index::{
    IndexValue, MaterializedSet, Tree, ViewCatalog, ViewMode, ViewSpec, eval_tree, key_in_tree,
};

use crate::ops_index::ShardSegs;
use crate::store::{Store, lock_write};

/// Store-level registry.
#[derive(Default)]
pub(crate) struct ViewReg {
    pub(crate) catalog: RwLock<(u64, ViewCatalog)>,
}

/// One shard's view states (inside `Inner`, guarded by the shard lock).
#[derive(Default)]
pub(crate) struct ShardViews {
    pub(crate) version: u64,
    pub(crate) views: Vec<ViewState>,
}

pub(crate) struct ViewState {
    spec: ViewSpec,
    mat: Option<MaterializedSet>,
    needs_rebuild: bool,
}

/// One page of view members plus the resume cursor.
pub type ViewPage = (Vec<(Vec<u8>, IndexValue)>, Option<(IndexValue, Vec<u8>)>);

const SIDECAR: &str = "view-catalog.meta";

impl Store {
    /// Declare a view (typed tree; `via` is not supported embedded —
    /// read fields in-process). Builds synchronously.
    pub fn view_create(
        &self,
        name: &[u8],
        tree: Tree,
        order_by: &[u8],
        desc: bool,
        mode: ViewMode,
    ) -> io::Result<()> {
        // Referenced indexes must exist.
        let mut names: Vec<Vec<u8>> = vec![order_by.to_vec()];
        tree.each_leaf(&mut |l| names.push(l.index.clone()));
        {
            let g = self
                .indexes
                .catalog
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for n in &names {
                if g.1.get(n).is_none() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "view references unknown index",
                    ));
                }
            }
        }
        let spec = ViewSpec {
            name: name.to_vec(),
            tree,
            order_by: order_by.to_vec(),
            desc,
            mode,
            via: None,
        };
        {
            let mut g = self
                .views
                .catalog
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (ver, cat) = &mut *g;
            cat.create(spec)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
            *ver += 1;
        }
        self.persist_view_sidecar();
        for shard in self.shards.iter() {
            let mut g = lock_write(shard);
            let inner = &mut *g;
            crate::ops_index::sync_segs(&self.indexes, &mut inner.idx_segs, &mut inner.store);
            sync_views(&self.views, &mut inner.view_segs, &inner.idx_segs);
        }
        Ok(())
    }

    /// Drop a view; `false` if absent.
    pub fn view_drop(&self, name: &[u8]) -> bool {
        let hit = {
            let mut g = self
                .views
                .catalog
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (ver, cat) = &mut *g;
            let hit = cat.drop_view(name);
            if hit {
                *ver += 1;
            }
            hit
        };
        if hit {
            self.persist_view_sidecar();
        }
        hit
    }

    /// Ordered page across shards (`after` resumes exclusively; DESC
    /// views page from the large end).
    pub fn view_query(
        &self,
        name: &[u8],
        after: Option<&(IndexValue, Vec<u8>)>,
        limit: usize,
    ) -> io::Result<ViewPage> {
        let limit = limit.clamp(1, 100_000);
        let mut desc = false;
        let mut all: Vec<(IndexValue, Vec<u8>)> = Vec::new();
        let mut found = false;
        for shard in self.shards.iter() {
            let mut g = lock_write(shard);
            let inner = &mut *g;
            crate::ops_index::sync_segs(&self.indexes, &mut inner.idx_segs, &mut inner.store);
            sync_views(&self.views, &mut inner.view_segs, &inner.idx_segs);
            let Some(vs) = inner.view_segs.views.iter_mut().find(|v| v.spec.name == name) else {
                continue;
            };
            found = true;
            desc = vs.spec.desc;
            if vs.needs_rebuild {
                rebuild(vs, &inner.idx_segs);
            }
            match &vs.mat {
                Some(m) => all.extend(m.page(after, limit, vs.spec.desc)),
                None => {
                    // Order-driven streaming (same clamp rationale as
                    // the server pager).
                    let r = resolver(&inner.idx_segs);
                    if let Some(order_seg) = r(&vs.spec.order_by) {
                        let cursor = after
                            .map(|(v, k)| kevy_index::Cursor { value: v.clone(), key: k.clone() });
                        let mut got = 0usize;
                        for (v, k) in order_seg.scan(cursor.as_ref(), vs.spec.desc) {
                            if key_in_tree(&vs.spec.tree, k, &&r) {
                                all.push((v.clone(), k.to_vec()));
                                got += 1;
                                if got == limit {
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
        if !found {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no such view"));
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
        let g = self
            .views
            .catalog
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.1.iter()
            .map(|s| (s.name.clone(), s.mode, s.tree.leaves()))
            .collect()
    }

    /// Summed member count across shards.
    pub fn view_count(&self, name: &[u8]) -> io::Result<u64> {
        Ok(self.view_query(name, None, 100_000)?.0.len() as u64)
    }

    fn persist_view_sidecar(&self) {
        let Some(dir) = &self.config.data_dir else { return };
        let g = self
            .views
            .catalog
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = dir.join("view-catalog.meta.tmp");
        if std::fs::write(&tmp, g.1.to_sidecar()).is_ok() {
            let _ = std::fs::rename(&tmp, dir.join(SIDECAR));
        }
    }

    /// Boot half — load the persisted view catalog.
    pub(crate) fn view_boot(&self) {
        let Some(dir) = &self.config.data_dir else { return };
        if let Ok(text) = std::fs::read_to_string(dir.join(SIDECAR))
            && let Some(cat) = ViewCatalog::from_sidecar(&text)
            && !cat.is_empty()
        {
            let mut g = self
                .views
                .catalog
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *g = (g.0 + 1, cat);
        }
    }
}

fn resolver<'a>(segs: &'a ShardSegs) -> impl Fn(&[u8]) -> Option<&'a kevy_index::Segment> {
    move |name: &[u8]| segs.segs.iter().find(|(s, _)| s.name == name).map(|(_, seg)| seg)
}

fn eval_shard(spec: &ViewSpec, segs: &ShardSegs) -> Vec<(IndexValue, Vec<u8>)> {
    let r = resolver(segs);
    let members = eval_tree(&spec.tree, &&r);
    members
        .into_iter()
        .filter_map(|k| r(&spec.order_by).and_then(|s| s.verify_entry(&k)).map(|v| (v.clone(), k)))
        .collect()
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
        if spec.desc {
            rows.reverse();
        }
        rows.truncate((top_k + top_k / 4) as usize);
    }
    for (v, k) in rows {
        mat.apply(&k, true, Some(v));
    }
    vs.needs_rebuild = false;
}

/// Reconcile with the catalog (under the shard lock).
pub(crate) fn sync_views(reg: &ViewReg, sv: &mut ShardViews, segs: &ShardSegs) {
    let g = reg
        .catalog
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (ver, cat) = &*g;
    if sv.version == *ver {
        return;
    }
    let mut next = Vec::new();
    for spec in cat.iter() {
        match sv.views.iter().position(|v| v.spec == *spec) {
            Some(i) => next.push(sv.views.swap_remove(i)),
            None => {
                let mat = match spec.mode {
                    ViewMode::Virtual => None,
                    ViewMode::Materialized { top_k } => Some(MaterializedSet::new(top_k, spec.desc)),
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
pub(crate) fn on_commit(
    reg: &ViewReg,
    sv: &mut ShardViews,
    segs: &ShardSegs,
    parts: &[&[u8]],
) {
    {
        let g = reg
            .catalog
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if g.1.is_empty() {
            return;
        }
    }
    sync_views(reg, sv, segs);
    let verb = parts.first().copied().unwrap_or(b"");
    if verb.eq_ignore_ascii_case(b"FLUSHALL") || verb.eq_ignore_ascii_case(b"FLUSHDB") {
        for vs in &mut sv.views {
            if let Some(m) = &mut vs.mat {
                m.clear();
            }
        }
        return;
    }
    // Same exact written-key walk as the index hook.
    crate::ops_index::each_written_key_pub(verb, parts, |key| {
        for vs in &mut sv.views {
            let Some(mat) = &mut vs.mat else { continue };
            let r = resolver(segs);
            let member = key_in_tree(&vs.spec.tree, key, &&r);
            let order = r(&vs.spec.order_by).and_then(|s| s.verify_entry(key)).cloned();
            if mat.apply(key, member, order) {
                vs.needs_rebuild = true;
            }
        }
    });
}
