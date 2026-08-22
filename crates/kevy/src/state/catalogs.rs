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
    /// Declarations only — its compiled indexes live in `index`, so
    /// shards need no per-table state and no generation.
    table: RwLock<Option<Arc<TableCatalog>>>,
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
            advise: Mutex::new(AdviseLog::new()),
            usage: RwLock::new(HashMap::new()),
        }
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
        self.advise
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .observe(name, shape, argv)
    }

    /// Is `name` a path the auto loop declared (any table's ledger)?
    pub(crate) fn is_auto_path(&self, name: &[u8]) -> bool {
        self.table()
            .is_some_and(|c| c.iter().any(|s| s.auto_added.iter().any(|e| e == name)))
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
        self.index
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
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
        self.view
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
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

    /// Snapshot the current table catalog (None = empty).
    pub(crate) fn table(&self) -> Option<Arc<TableCatalog>> {
        self.table
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
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
        *self
            .catalogs
            .index
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(c));
        self.catalogs.index_gen.fetch_add(1, Ordering::Release);
        self.bump_control_epoch();
        self.catalogs.advise_clear();
        self.catalogs.usage_rekey(names, (kevy_store::now_unix_ms() / 1000) as i64);
    }

    /// Swap in a new view catalog — same protocol as
    /// [`Self::install_index_catalog`].
    pub(crate) fn install_view_catalog(&self, c: ViewCatalog) {
        *self
            .catalogs
            .view
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(c));
        self.catalogs.view_gen.fetch_add(1, Ordering::Release);
        self.bump_control_epoch();
    }

    /// Swap in a new table catalog. No generation, no epoch: shards
    /// carry no per-table state — a table's runtime footprint is its
    /// compiled indexes, installed via
    /// [`Self::install_index_catalog`] in the same command.
    pub(crate) fn install_table_catalog(&self, c: TableCatalog) {
        *self
            .catalogs
            .table
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(c));
        self.catalogs.advise_clear();
    }
}
