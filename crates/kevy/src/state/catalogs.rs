//! Process-wide catalogs owned by [`RuntimeState`].
//!
//! [`RuntimeState`]: crate::RuntimeState

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use kevy_index::{Catalog, ViewCatalog};

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
}

impl CatalogState {
    pub(crate) fn new() -> Self {
        Self {
            scripts: Mutex::new(HashMap::new()),
            index: RwLock::new(None),
            index_gen: AtomicU64::new(0),
            view: RwLock::new(None),
            view_gen: AtomicU64::new(0),
        }
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
}

impl RuntimeState {
    /// Swap in a new index catalog (IDX.CREATE / IDX.DROP / sidecar
    /// boot). Bumps the generation (shards refresh their segment
    /// lists lazily), then the control epoch (writer protocol step ②
    /// — every shard's gate bits re-derive `IDX_NONEMPTY` on their
    /// next command).
    pub(crate) fn install_index_catalog(&self, c: Catalog) {
        *self
            .catalogs
            .index
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(c));
        self.catalogs.index_gen.fetch_add(1, Ordering::Release);
        self.bump_control_epoch();
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
}
