//! The index, view and table catalog as recorded state, the way the
//! server keeps it: every change is recorded as one internal frame
//! carrying the whole catalog, `XINTERNAL.CATALOG <lineage> <version>
//! <index> <view> <table>`, through the write path (log, replicas, feed);
//! every snapshot and rewritten log carries the current one; a frame is
//! applied when it is newer than what the store holds, and a replica's
//! full sync replaces its catalog with its primary's. A server replica
//! applies the frames an embedded primary records, and the other way
//! round.

use std::sync::{Arc, Mutex, PoisonError};

use kevy_index::{Catalog, TableCatalog, ViewCatalog};
use kevy_resp::Argv;

use crate::KevyResult;
use crate::ops_index::IndexReg;
use crate::ops_table::TableReg;
use crate::ops_view::ViewReg;
use crate::store::{Store, lock_write};

/// The three registries and where the catalog stands as recorded state,
/// shared by the store and every shard (the replica runner and the
/// snapshot writers reach it through a shard).
#[derive(Debug)]
pub(crate) struct CatalogRegs {
    pub(crate) indexes: Arc<IndexReg>,
    pub(crate) views: Arc<ViewReg>,
    pub(crate) tables: Arc<TableReg>,
    /// `(lineage, version)`; lineage 0 = nothing recorded yet.
    at: Mutex<(u64, u64)>,
}

impl CatalogRegs {
    pub(crate) fn new(tables: Arc<TableReg>) -> Self {
        Self { indexes: Arc::default(), views: Arc::default(), tables, at: Mutex::new((0, 0)) }
    }

    fn texts(&self) -> [String; 3] {
        [
            self.indexes.catalog.read().unwrap_or_else(PoisonError::into_inner).1.to_sidecar(),
            self.views.catalog.read().unwrap_or_else(PoisonError::into_inner).1.to_sidecar(),
            self.tables.catalog.read().unwrap_or_else(PoisonError::into_inner).to_sidecar(),
        ]
    }

    fn frame(&self, at: (u64, u64)) -> Vec<Vec<u8>> {
        let [index, view, table] = self.texts();
        vec![
            kevy_resp::ops_table::CATALOG.as_bytes().to_vec(),
            at.0.to_string().into_bytes(),
            at.1.to_string().into_bytes(),
            index.into_bytes(),
            view.into_bytes(),
            table.into_bytes(),
        ]
    }

    #[cfg(feature = "persist")]
    fn at(&self) -> (u64, u64) {
        *self.at.lock().unwrap_or_else(PoisonError::into_inner)
    }

    #[cfg(feature = "persist")]
    /// The frame a snapshot or a rewritten log keeps beside the keyspace.
    pub(crate) fn aux(&self) -> Option<Argv> {
        let at = self.at();
        (at.0 != 0).then(|| Argv::from(self.frame(at)))
    }

    /// Take a catalog frame when it is newer than what the store holds,
    /// or, on a full sync, whenever it comes from another lineage; `None`
    /// on a full sync empties the catalog. A malformed frame is skipped,
    /// the way a replay skips a frame it cannot apply.
    pub(crate) fn adopt(&self, frame: Option<&Argv>, full_sync: bool) {
        let mut held = self.at.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(frame) = frame else {
            if full_sync && held.0 != 0 {
                self.install(Catalog::new(), ViewCatalog::new(), TableCatalog::new());
                *held = (0, 0);
            }
            return;
        };
        let Some((at, icat, vcat, tcat)) = decode(frame) else { return };
        if at > *held || full_sync && at.0 != held.0 {
            self.install(icat, vcat, tcat);
            *held = at;
        }
    }

    /// Install each catalog that differs from the one held; the shards
    /// rebuild what changed on their next touch.
    fn install(&self, icat: Catalog, vcat: ViewCatalog, tcat: TableCatalog) {
        let [index, view, table] = self.texts();
        if index != icat.to_sidecar() {
            let mut g = self.indexes.catalog.write().unwrap_or_else(PoisonError::into_inner);
            *g = (g.0 + 1, icat);
        }
        if table != tcat.to_sidecar() {
            *self.tables.catalog.write().unwrap_or_else(PoisonError::into_inner) = tcat;
            self.tables.advise.lock().unwrap_or_else(PoisonError::into_inner).clear();
        }
        if view != vcat.to_sidecar() {
            let mut g = self.views.catalog.write().unwrap_or_else(PoisonError::into_inner);
            *g = (g.0 + 1, vcat);
        }
    }

    /// Move to the next version, minting a lineage on the first.
    fn next_version(&self) -> (u64, u64) {
        let mut at = self.at.lock().unwrap_or_else(PoisonError::into_inner);
        if at.0 == 0 {
            at.0 = kevy_store::now_unix_ms().max(1);
        }
        at.1 += 1;
        *at
    }
}

/// `(lineage, version)` and the three catalogs a frame carries.
fn decode(frame: &Argv) -> Option<((u64, u64), Catalog, ViewCatalog, TableCatalog)> {
    let num = |i: usize| std::str::from_utf8(frame.get(i)?).ok()?.parse::<u64>().ok();
    let text = |i: usize| std::str::from_utf8(frame.get(i)?).ok();
    let (index, view, table) = (text(3)?, text(4)?, text(5)?);
    let icat = if index.is_empty() { Catalog::new() } else { Catalog::from_sidecar(index)? };
    let vcat = if view.is_empty() { ViewCatalog::new() } else { ViewCatalog::from_sidecar(view)? };
    let tcat =
        if table.is_empty() { TableCatalog::new() } else { TableCatalog::from_sidecar(table)? };
    Some(((num(1)?, num(2)?), icat, vcat, tcat))
}

impl Store {
    /// Run one catalog command: refused on a replica or a closed store,
    /// and recorded once when it changed the catalog (a failure part way
    /// through records what it left).
    pub(crate) fn catalog_change<T>(&self, f: impl FnOnce() -> KevyResult<T>) -> KevyResult<T> {
        crate::store::ensure_writable(self)?;
        let before = self.guard.catalog.texts();
        let out = f();
        if self.guard.catalog.texts() != before {
            self.record_catalog()?;
        }
        out
    }

    /// Record the catalog as it now stands, through shard 0's write path.
    fn record_catalog(&self) -> KevyResult<()> {
        let regs = &self.guard.catalog;
        let frame = regs.frame(regs.next_version());
        let parts: Vec<&[u8]> = frame.iter().map(Vec::as_slice).collect();
        crate::store::commit_write(&mut lock_write(&self.shards[0]), &parts)
    }
}

#[cfg(all(test, feature = "persist"))]
#[path = "catalog_record_tests.rs"]
mod tests;

#[cfg(feature = "persist")]
mod sidecars {
    use std::path::Path;

    use kevy_index::{Catalog, TableCatalog, ViewCatalog};

    use crate::KevyResult;
    use crate::store::{Store, lock_write};

    /// The files a 6.4 directory kept the catalog in.
    const SIDECARS: [&str; 3] = ["index-catalog.meta", "view-catalog.meta", "table-catalog.meta"];

    /// What one sidecar holds: empty when absent, `None` when present but
    /// unreadable.
    fn read<T: Default>(dir: &Path, name: &str, parse: impl Fn(&str) -> Option<T>) -> Option<T> {
        match std::fs::read_to_string(dir.join(name)) {
            Ok(text) => parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(T::default()),
            Err(_) => None,
        }
    }

    impl Store {
        /// Open-time settling of the files a 6.4 directory kept its
        /// catalog in: superseded when the logs or snapshots held a
        /// catalog, otherwise read once and recorded, and removed once
        /// the record is on disk.
        pub(crate) fn settle_sidecars(&self) -> KevyResult<()> {
            let Some(dir) = self.config.data_dir.clone() else { return Ok(()) };
            if self.guard.catalog.at().0 == 0 && !self.import_sidecars(&dir)? {
                return Ok(());
            }
            for name in SIDECARS {
                // absent is the usual case
                drop(std::fs::remove_file(dir.join(name)));
            }
            Ok(())
        }

        /// Install and record what the sidecars in `dir` hold; `true` once
        /// the record is on disk, or when there was nothing to import. A
        /// replica takes its catalog from its primary, and a sidecar that
        /// does not parse is left in place for someone to look at.
        fn import_sidecars(&self, dir: &Path) -> KevyResult<bool> {
            #[cfg(all(feature = "replicate", not(target_arch = "wasm32")))]
            if self.is_replica() {
                return Ok(false);
            }
            let icat = read(dir, SIDECARS[0], Catalog::from_sidecar);
            let vcat = read(dir, SIDECARS[1], ViewCatalog::from_sidecar);
            let tcat = read(dir, SIDECARS[2], TableCatalog::from_sidecar);
            let (Some(icat), Some(vcat), Some(tcat)) = (icat, vcat, tcat) else {
                eprintln!(
                    "kevy: a catalog file in {} does not parse; it stays in place, not imported",
                    dir.display()
                );
                return Ok(false);
            };
            if icat.is_empty() && vcat.is_empty() && tcat.is_empty() {
                return Ok(true);
            }
            self.guard.catalog.install(icat, vcat, tcat);
            self.record_catalog()?;
            match lock_write(&self.shards[0]).aof.as_mut() {
                Some(aof) => {
                    aof.sync_now()?;
                    Ok(true)
                }
                None => Ok(false),
            }
        }
    }
}
