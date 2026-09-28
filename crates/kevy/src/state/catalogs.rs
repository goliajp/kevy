//! Process-wide catalogs owned by [`RuntimeState`].
//!
//! [`RuntimeState`]: crate::RuntimeState

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use kevy_index::{
    AdviseEntry, AdviseLog, AdviseShape, Catalog, TableCatalog, UsageCell, ViewCatalog,
};

use super::RuntimeState;

#[derive(Debug)]
pub(crate) struct CatalogState {
    /// Script cache shared across all shards: SCRIPT LOAD / EVAL write
    /// here, EVALSHA reads here and forwards the source to the
    /// per-shard `LuaHost` (so the per-shard VM pool still runs the
    /// script — thread-locality preserved). Cross-shard by design:
    /// a `SCRIPT LOAD` served on shard X must satisfy an `EVALSHA`
    /// routed to shard Y.
    pub(crate) scripts: Mutex<HashMap<[u8; 20], Vec<u8>>>,
    /// The index catalog (IDX.CREATE / IDX.DROP / sidecar boot).
    /// `None` = never installed. Cold-path lock: the per-command hot
    /// path reads the generation below and each shard's cached
    /// segment list instead.
    index: RwLock<Option<Arc<Catalog>>>,
    /// Bumped (Release) on every index-catalog install; shards
    /// rebuild their `ShardIndexes` lazily when it moves.
    index_gen: AtomicU64,
    /// The view catalog — same lifecycle as `index`.
    view: RwLock<Option<Arc<ViewCatalog>>>,
    /// Bumped (Release) on every view-catalog install.
    view_gen: AtomicU64,
    /// The table catalog (TABLE.DECLARE / TABLE.DROP / sidecar boot).
    table: RwLock<Option<Arc<TableCatalog>>>,
    /// Bumped (Release) on every table-catalog install. A table used to
    /// carry no per-shard state — its runtime footprint was its compiled
    /// indexes — but the packed representation gave it one: a declaration
    /// has to reach the rows that were already there, and a shard learns a
    /// new declaration exists by this moving.
    table_gen: AtomicU64,
    /// The refusal log (the auto-declaration loop's observation
    /// face): written at the origin reduce when a query is refused
    /// for a missing declaration, read by `IDX.ADVISE`. Cleared on
    /// every catalog install — a family the new catalog serves stops
    /// being refused, and one it doesn't re-earns its seat on the
    /// next refusal. Cold path only (refusals and an admin verb).
    advise: Mutex<AdviseLog>,
    /// The refusal log's dual: per declared path, how often it
    /// serves (the reclaim face's raw material). Rebuilt on install,
    /// KEEPING same-name cells — "unused since declare" must survive
    /// unrelated catalog changes. The served-query path pays one
    /// uncontended read-lock and two relaxed stores.
    usage: RwLock<HashMap<Vec<u8>, Arc<UsageCell>>>,
    /// Per global index, which incarnation of it the catalog holds: a new
    /// number whenever its spec or partitioning changes, or it is dropped
    /// and created again. Every message between shards carries it, so one
    /// sent for an earlier incarnation is never applied to a later one.
    incarnations: Mutex<(u64, HashMap<Vec<u8>, u64>)>,
}

impl CatalogState {
    pub(crate) fn new() -> Self {
        Self {
            scripts: Mutex::new(HashMap::new()),
            index: RwLock::new(None),
            index_gen: AtomicU64::new(0),
            view: RwLock::new(None),
            view_gen: AtomicU64::new(0),
            table: RwLock::new(None),
            table_gen: AtomicU64::new(0),
            advise: Mutex::new(AdviseLog::new()),
            usage: RwLock::new(HashMap::new()),
            incarnations: Mutex::new((0, HashMap::new())),
        }
    }

    /// The incarnation of global index `name` in the installed catalog
    /// (0 for a local or unknown one).
    pub(crate) fn incarnation(&self, name: &[u8]) -> u64 {
        let incs = self.incarnations.lock().unwrap_or_else(PoisonError::into_inner);
        incs.1.get(name).copied().unwrap_or(0)
    }

    /// Number the global indexes of catalog `new`: one unchanged since `old`
    /// keeps its incarnation, any other gets a fresh one.
    fn number_incarnations(&self, old: Option<&Catalog>, new: &Catalog) {
        let mut incs = self.incarnations.lock().unwrap_or_else(PoisonError::into_inner);
        let (next, prev) = &mut *incs;
        let mut map = HashMap::new();
        for (spec, _) in new.iter() {
            let part = new.partitioning(&spec.name);
            if !part.is_global() {
                continue;
            }
            let same = old.is_some_and(|o| {
                o.get(&spec.name).is_some_and(|(s, _)| s == spec)
                    && o.partitioning(&spec.name) == part
            });
            let inc = match (same, prev.get(&spec.name)) {
                (true, Some(&inc)) => inc,
                _ => {
                    *next += 1;
                    *next
                }
            };
            map.insert(spec.name.clone(), inc);
        }
        *prev = map;
    }

    /// The usage cell for a declared path (None = not declared).
    pub(crate) fn usage_cell(&self, name: &[u8]) -> Option<Arc<UsageCell>> {
        self.usage.read().unwrap_or_else(PoisonError::into_inner).get(name).cloned()
    }

    /// Every declared path's `(name, hits, last_hit_s, declared_s,
    /// min_margin)`.
    pub(crate) fn usage_snapshot(&self) -> Vec<(Vec<u8>, u64, i64, i64, i64)> {
        self.usage
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(n, c)| {
                let (hits, last, declared) = c.read();
                let margin = c.min_margin.load(std::sync::atomic::Ordering::Relaxed);
                (n.clone(), hits, last, declared, margin)
            })
            .collect()
    }

    /// Re-key the usage table to `names`, keeping same-name cells —
    /// counters survive unrelated installs, dropped paths drop, new
    /// paths date from `now_s`.
    fn usage_rekey(&self, names: Vec<Vec<u8>>, now_s: i64) {
        let mut g = self.usage.write().unwrap_or_else(PoisonError::into_inner);
        let old = std::mem::take(&mut *g);
        for n in names {
            let cell =
                old.get(&n).cloned().unwrap_or_else(|| Arc::new(UsageCell::declared_at(now_s)));
            g.insert(n, cell);
        }
    }

    /// Record one refused declaration family; returns its count
    /// after this observation (the auto loop's threshold input).
    pub(crate) fn advise_observe(&self, name: &[u8], shape: AdviseShape, argv: &[Vec<u8>]) -> u64 {
        self.advise.lock().unwrap_or_else(PoisonError::into_inner).observe(name, shape, argv)
    }

    /// Is `name` a path the auto loop declared (any table's ledger)?
    pub(crate) fn is_auto_path(&self, name: &[u8]) -> bool {
        self.table().is_some_and(|c| c.iter().any(|s| s.auto_added.iter().any(|e| e == name)))
    }

    /// Snapshot the observed refusal families, most-refused first.
    pub(crate) fn advise_entries(&self) -> Vec<AdviseEntry> {
        self.advise
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entries()
            .into_iter()
            .cloned()
            .collect()
    }

    /// Forget every observed refusal (a catalog just installed).
    fn advise_clear(&self) {
        self.advise.lock().unwrap_or_else(PoisonError::into_inner).clear();
    }

    /// Snapshot the current index catalog (None = empty).
    pub(crate) fn index(&self) -> Option<Arc<Catalog>> {
        self.index.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Is at least one index declared? Cold-path input to the
    /// per-shard `IDX_NONEMPTY` gate bit — the hot path reads the
    /// cached bit, never this lock.
    pub(crate) fn index_nonempty(&self) -> bool {
        self.index
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .is_some_and(|c| !c.is_empty())
    }

    /// Whether any table is declared — the gate for the packed
    /// representation, which a table earns by declaring columns whether or
    /// not it also declares an index.
    pub(crate) fn table_nonempty(&self) -> bool {
        self.table
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .is_some_and(|c| !c.is_empty())
    }

    /// The index-catalog generation (Acquire — pairs with the install
    /// bump so a moved value guarantees the new catalog is visible).
    pub(crate) fn index_gen(&self) -> u64 {
        self.index_gen.load(Ordering::Acquire)
    }

    /// Snapshot the current view catalog (None = empty).
    pub(crate) fn view(&self) -> Option<Arc<ViewCatalog>> {
        self.view.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Is at least one view declared? Cold-path input to the
    /// per-shard `VIEW_NONEMPTY` gate bit.
    pub(crate) fn view_nonempty(&self) -> bool {
        self.view
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .is_some_and(|c| !c.is_empty())
    }

    /// The view-catalog generation (Acquire).
    pub(crate) fn view_gen(&self) -> u64 {
        self.view_gen.load(Ordering::Acquire)
    }

    /// The table-catalog generation (Acquire).
    pub(crate) fn table_gen(&self) -> u64 {
        self.table_gen.load(Ordering::Acquire)
    }

    /// Snapshot the current table catalog (None = empty).
    pub(crate) fn table(&self) -> Option<Arc<TableCatalog>> {
        self.table.read().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

impl RuntimeState {
    /// Swap in a new index catalog (IDX.CREATE / IDX.DROP / sidecar
    /// boot). Bumps the generation (shards refresh their segment
    /// lists lazily), then the control epoch (writer protocol step ②
    /// — every shard's gate bits re-derive `IDX_NONEMPTY` on their
    /// next command).
    pub(crate) fn install_index_catalog(&self, c: Catalog) {
        let names: Vec<Vec<u8>> = c.iter().map(|(s, _)| s.name.clone()).collect();
        // numbered before the generation moves, so a shard that sees the
        // new catalog reads the incarnations that go with it
        self.catalogs.number_incarnations(self.catalogs.index().as_deref(), &c);
        *self.catalogs.index.write().unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(c));
        self.catalogs.index_gen.fetch_add(1, Ordering::Release);
        self.bump_control_epoch();
        self.catalogs.advise_clear();
        self.catalogs.usage_rekey(names, (kevy_store::now_unix_ms() / 1000) as i64);
    }

    /// Swap in a new view catalog — same protocol as
    /// [`Self::install_index_catalog`].
    pub(crate) fn install_view_catalog(&self, c: ViewCatalog) {
        *self.catalogs.view.write().unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(c));
        self.catalogs.view_gen.fetch_add(1, Ordering::Release);
        self.bump_control_epoch();
    }

    /// Swap in a new table catalog, and tell the shards a declaration
    /// changed so the packing backfill picks up the rows that preceded it.
    /// Moves the control epoch like the other installs: `TABLE_NONEMPTY`
    /// derives from this catalog, and a shard that re-read its gate after
    /// the index catalog's install would otherwise keep a gate without it.
    pub(crate) fn install_table_catalog(&self, c: TableCatalog) {
        *self.catalogs.table.write().unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(c));
        self.catalogs.table_gen.fetch_add(1, Ordering::Release);
        self.bump_control_epoch();
        self.catalogs.advise_clear();
    }
}

#[cfg(test)]
mod tests {
    use kevy_index::{IndexKind, IndexSpec, Partitioning, ValType, order_key};

    use super::*;

    fn with(global: &[&[u8]], split: &[u8]) -> Catalog {
        let mut c = Catalog::new();
        for name in global {
            let spec = IndexSpec::single_field(
                name.to_vec(),
                b"u:".to_vec(),
                b"age".to_vec(),
                ValType::I64,
                IndexKind::Range,
            );
            let splits = vec![order_key(ValType::I64, split).unwrap()];
            c.create_with(spec, Partitioning::Global { splits }).unwrap();
        }
        c
    }

    #[test]
    fn an_index_keeps_its_incarnation_only_while_it_stays_the_same() {
        let cats = CatalogState::new();
        let number = |old: Option<&Catalog>, new: &Catalog| cats.number_incarnations(old, new);
        let first = with(&[b"a", b"b"], b"10");
        number(None, &first);
        let (a, b) = (cats.incarnation(b"a"), cats.incarnation(b"b"));
        assert!(a > 0 && b > 0 && a != b);
        // another index created: these two unchanged
        let more = with(&[b"a", b"b", b"c"], b"10");
        number(Some(&first), &more);
        assert_eq!((cats.incarnation(b"a"), cats.incarnation(b"b")), (a, b));
        // split points moved (a rebuild): every one of them is new
        let moved = with(&[b"a", b"b", b"c"], b"20");
        number(Some(&more), &moved);
        assert!(cats.incarnation(b"a") > a && cats.incarnation(b"b") > b);
        // dropped, then created the same again: new
        let (a2, dropped) = (cats.incarnation(b"a"), with(&[b"b", b"c"], b"20"));
        number(Some(&moved), &dropped);
        assert_eq!(cats.incarnation(b"a"), 0);
        number(Some(&dropped), &moved);
        assert!(cats.incarnation(b"a") > a2);
    }
}
