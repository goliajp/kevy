//! Multi-database keyspace holder: one `Store` per logical db, selected
//! per connection by `SELECT`. Design: `docs/multi-db.md` (kevy repo).
//!
//! A shard is single-threaded, so the "current db" is a `Cell<u16>` set at
//! the top of each client-command dispatch ([`Shard::handle_command`]) and
//! reset to 0 on internal (replication/persist/tick) entries. Every existing
//! `self.store.…` call site then transparently operates on the selected db
//! through `Deref`/`DerefMut` — no per-site changes.
//!
//! Phase 1 scope (this PR): in-memory multi-DB only. AOF/snapshot and
//! replication are refused at boot when `databases > 1` (see
//! `kevy/src/main.rs::validate_databases`) — replaying a single-db AOF into
//! a multi-db keyspace would silently merge dbs, so Phase 2 must tag the
//! log with `SELECT` frames (or write per-db AOFs) before enabling it.

use std::cell::Cell;
use std::ops::{Deref, DerefMut};

use kevy_persist::SnapshotSource;
use kevy_store::{Store, Value};

/// Per-shard set of logical databases. `stores[0]` is the primary; the
/// vector is always exactly `databases` long (stores are cheap to create —
/// an empty keyspace allocates no table).
pub(crate) struct DbStores {
    stores: Vec<Store>,
    cur: Cell<u16>,
}

impl DbStores {
    /// `databases` logical dbs, each a fresh `Store` stamped with its index.
    pub(crate) fn new(databases: u16) -> Self {
        let n = databases as usize;
        let mut stores = Vec::with_capacity(n);
        for db in 0..n {
            let mut s = Store::new();
            s.set_db_index(db as u16);
            stores.push(s);
        }
        Self { stores, cur: Cell::new(0) }
    }

    pub(crate) fn databases(&self) -> u16 {
        self.stores.len() as u16
    }

    /// The db selected on the connection currently being dispatched.
    pub(crate) fn cur(&self) -> u16 {
        self.cur.get()
    }

    /// Select the db for the next dispatch on this (single-threaded) shard.
    pub(crate) fn set_cur(&self, db: u16) {
        debug_assert!((db as usize) < self.stores.len());
        self.cur.set(db);
    }

    /// Mutable access to one db's store, whatever the current selection.
    pub(crate) fn store_mut(&mut self, db: u16) -> &mut Store {
        &mut self.stores[db as usize]
    }

    /// Shared access to one db's store (persistence views, INFO gauges).
    pub(crate) fn store(&self, db: u16) -> &Store {
        &self.stores[db as usize]
    }

    /// `SELECT <db>`: returns `true` when `db` is a valid index.
    pub(crate) fn select(&self, db: u16) -> bool {
        if (db as usize) < self.stores.len() {
            self.set_cur(db);
            true
        } else {
            false
        }
    }

    /// `FLUSHALL`: flush every db. Returns per-db evicted counts is
    /// overkill; callers only need the side effect + the watched-key bump.
    pub(crate) fn flush_all(&mut self) {
        for s in &mut self.stores {
            s.flushall();
            s.bump_all_watched();
        }
    }

    /// `SWAPDB a b`: swap the two keyspaces.
    pub(crate) fn swap(&mut self, a: u16, b: u16) {
        if a == b {
            return;
        }
        self.stores.swap(a as usize, b as usize);
        let (lo, hi) = if a < b { (a as usize, b as usize) } else { (b as usize, a as usize) };
        let (left, right) = self.stores.split_at_mut(hi);
        let (sa, sb) = (&mut left[lo], &mut right[0]);
        // Watch registrations stay attached to their logical db slot
        // (Redis keeps watched_keys per dbid and does not move them on
        // swap): un-swap the side maps, then dirty every watched key in
        // BOTH dbs, so a pending EXEC on either side aborts.
        sa.swap_watch_versions(sb);
        sa.bump_all_watched();
        sb.bump_all_watched();
        // Keep the db_index stamp honest after the swap.
        let (ia, ib) = (sa.db_index(), sb.db_index());
        sa.set_db_index(ib);
        sb.set_db_index(ia);
    }
}

impl Deref for DbStores {
    type Target = Store;
    /// The currently selected db's store — the hot path: every
    /// `self.store.get(…)` in the shard lands here.
    #[inline]
    fn deref(&self) -> &Store {
        &self.stores[self.cur.get() as usize]
    }
}

impl DerefMut for DbStores {
    #[inline]
    fn deref_mut(&mut self) -> &mut Store {
        let cur = self.cur.get() as usize;
        &mut self.stores[cur]
    }
}

impl DbStores {
    /// Enable row segments on every db, each in its own directory
    /// (`segs-<shard>` for db 0 — the historical layout — and
    /// `segs-<shard>-db<N>` for db N > 0, so per-db manifests never
    /// collide).
    pub(crate) fn enable_seg_rows_all(
        &mut self,
        data_dir: &std::path::Path,
        shard: usize,
    ) -> Result<(), kevy_store::SegRowsError> {
        for db in 0..self.stores.len() {
            let dir = kevy_persist::layout::segs_dir_db(data_dir, shard, db as u16);
            self.stores[db].enable_seg_rows(&dir)?;
        }
        Ok(())
    }

    /// Apply a SEGMENTED stitch frame to the currently selected db, in
    /// that db's own segment directory (AOF replay: a `SELECT` frame
    /// precedes the db's frames, so the cursor is already right).
    pub(crate) fn apply_segmented_cur(
        &mut self,
        data_dir: &std::path::Path,
        shard: usize,
        file: &[u8],
    ) -> Result<u64, kevy_store::SegRowsError> {
        let dir = kevy_persist::layout::segs_dir_db(data_dir, shard, self.cur());
        self.deref_mut().apply_segmented(&dir, file)
    }

    /// Sweep orphan row segments on every db (boot, post-restore).
    pub(crate) fn sweep_orphan_row_segs_all(&mut self) {
        for s in &mut self.stores {
            s.sweep_orphan_row_segs();
        }
    }

    /// Demote every db to its tier watermark (replay + boot).
    pub(crate) fn demote_all_to_watermark(&mut self) {
        for s in &mut self.stores {
            s.demote_to_watermark();
        }
    }

    /// Freeze one COW snapshot view per db — the SAVE/BGSAVE and AOF
    /// rewrite inputs (multi-db persistence writes one image per db).
    /// Snapshot views for the replication ship, db 0 first. The db-0 ship
    /// flushes the whole replica keyspace, so only NON-EMPTY dbs > 0 need
    /// shipping — empty dbs are already cleared by the db-0 flush, and a
    /// single-db-shaped primary (everything in db 0) ships exactly one
    /// stream, byte-identical to the historical `+SNAPSHOT` flow.
    pub(crate) fn collect_snapshot_all(&self) -> Vec<(u16, kevy_store::SnapshotView)> {
        self.stores
            .iter()
            .enumerate()
            .filter(|(db, s)| *db == 0 || s.dbsize() > 0)
            .map(|(db, s)| (db as u16, s.collect_snapshot()))
            .collect()
    }
}

/// Persistence sees every db: snapshots are written per db (db 0 keeps
/// the historical `dump-<shard>.rdb` file, db N gets `dump-<shard>-db<N>.rdb`)
/// and the AOF rewrite iterates dbs through the `*_in_db` hooks below,
/// emitting a `SELECT <db>` marker per db (see `kevy_persist::dump_aof`).
/// `for_each_entry` itself stays "current db" for single-db consumers
/// (reshard merge); multi-db writers must use the `*_in_db` hooks.
impl SnapshotSource for DbStores {
    fn for_each_entry(&self, f: impl FnMut(&[u8], &Value, Option<u64>)) {
        self.deref().for_each_entry(f)
    }

    fn db_count(&self) -> u16 {
        self.databases()
    }

    fn for_each_entry_in_db(&self, db: u16, f: impl FnMut(&[u8], &Value, Option<u64>)) {
        self.stores[db as usize].for_each_entry(f)
    }

    fn row_seg_files_in_db(&self, db: u16) -> Vec<(u32, String)> {
        self.stores[db as usize].row_seg_files()
    }

    fn for_each_hash_ttl_in_db(&self, db: u16, f: impl FnMut(&[u8], &[u8], u64)) {
        self.stores[db as usize].for_each_hash_ttl(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_isolates_keyspaces() {
        let mut dbs = DbStores::new(4);
        dbs.set_cur(0);
        dbs.set(b"k", b"v0".to_vec(), None, kevy_store::SetCondition::Always);
        assert!(dbs.select(2));
        assert_eq!(dbs.cur(), 2);
        assert_eq!(dbs.get(b"k").unwrap(), None);
        dbs.set(b"k", b"v2".to_vec(), None, kevy_store::SetCondition::Always);
        assert_eq!(dbs.get(b"k").unwrap().as_deref(), Some(b"v2".as_slice()));
        assert!(!dbs.select(4));
        assert_eq!(dbs.cur(), 2, "failed select must not move the cursor");
        dbs.set_cur(0);
        assert_eq!(dbs.get(b"k").unwrap().as_deref(), Some(b"v0".as_slice()));
    }

    #[test]
    fn swapdb_swaps_contents_and_stamps() {
        let mut dbs = DbStores::new(3);
        dbs.set_cur(0);
        dbs.set(b"a", b"0".to_vec(), None, kevy_store::SetCondition::Always);
        dbs.set_cur(1);
        dbs.set(b"a", b"1".to_vec(), None, kevy_store::SetCondition::Always);
        dbs.swap(0, 1);
        dbs.set_cur(0);
        assert_eq!(dbs.get(b"a").unwrap().as_deref(), Some(b"1".as_slice()));
        assert_eq!(dbs.store(0).db_index(), 0);
        dbs.set_cur(1);
        assert_eq!(dbs.get(b"a").unwrap().as_deref(), Some(b"0".as_slice()));
    }

    #[test]
    fn swapdb_dirties_watches_on_both_dbs_but_keeps_them_per_slot() {
        let mut dbs = DbStores::new(3);
        // WATCH k on db0 and db1: version 0 recorded in each slot.
        dbs.set_cur(0);
        dbs.store_mut(0).record_watch(b"k");
        dbs.set_cur(1);
        dbs.store_mut(1).record_watch(b"k");
        let v0 = dbs.store(0).key_version(b"k");
        let v1 = dbs.store(1).key_version(b"k");
        dbs.swap(0, 1);
        // Watch registrations stay attached to their logical db slot
        // and both were dirtied by the swap (Redis: touchAllWatchedKeysForDb
        // on both dbs) — an EXEC on either side must abort.
        assert_ne!(dbs.store(0).key_version(b"k"), v0);
        assert_ne!(dbs.store(1).key_version(b"k"), v1);
        // swap(a, a) is a no-op that must not dirty anything.
        let before = dbs.store(2).key_version(b"z");
        dbs.store_mut(2).record_watch(b"z");
        let v2 = dbs.store(2).key_version(b"z");
        dbs.swap(2, 2);
        assert_eq!(dbs.store(2).key_version(b"z"), v2);
        assert_eq!(before, 0);
    }

    #[test]
    fn flushall_clears_every_db() {
        let mut dbs = DbStores::new(3);
        for db in 0..3u16 {
            dbs.set_cur(db);
            dbs.set(b"k", vec![db as u8], None, kevy_store::SetCondition::Always);
        }
        dbs.flush_all();
        for db in 0..3u16 {
            dbs.set_cur(db);
            assert_eq!(dbs.get(b"k").unwrap(), None);
        }
    }
}

#[cfg(test)]
mod tests_phase2 {
    use super::*;
    use kevy_persist::SnapshotSource;

    #[test]
    fn snapshot_source_db_hooks_expose_each_db() {
        let mut dbs = DbStores::new(3);
        dbs.set_cur(0);
        dbs.set(b"k", b"v0".to_vec(), None, kevy_store::SetCondition::Always);
        dbs.set_cur(2);
        dbs.set(b"k", b"v2".to_vec(), None, kevy_store::SetCondition::Always);
        assert_eq!(SnapshotSource::db_count(&dbs), 3);
        let mut seen = Vec::new();
        dbs.for_each_entry_in_db(2, |k, v, _| {
            seen.push((k.to_vec(), format!("{:?}", v)));
        });
        assert_eq!(seen.len(), 1);
        dbs.set_cur(2);
        assert_eq!(dbs.get(b"k").unwrap().as_deref(), Some(b"v2".as_slice()));
        assert_eq!(dbs.store(1).dbsize(), 0);
        // collect_snapshot_all freezes db 0 plus every NON-EMPTY db (the
        // db-0 ship flushes the whole replica keyspace, so empty dbs > 0
        // need no stream)
        let views = dbs.collect_snapshot_all();
        assert_eq!(views.len(), 2);
        assert_eq!(views[0].0, 0);
        assert_eq!(views[1].0, 2);
    }
}
