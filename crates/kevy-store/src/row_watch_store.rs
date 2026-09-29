//! [`Store`]'s side of the row recorder: setting the watch and taking the
//! record.

use crate::Store;
#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use crate::row_watch::{Journal, RowChanges, RowWatch};

impl Store {
    /// Record, from now on, the watched fields of every row under the
    /// watch's prefixes before its first write since the last
    /// [`Store::take_row_changes`]. An empty watch switches recording off;
    /// replacing the watch drops what was recorded under the old one.
    ///
    /// ```
    /// use kevy_store::{RowWatch, Store};
    /// let mut s = Store::new();
    /// s.set_row_watch(RowWatch::new().with_prefix("u:", vec![b"age".to_vec()]));
    /// assert!(s.row_watch().is_some_and(|w| w.rules() == 1));
    /// s.set_row_watch(RowWatch::new());
    /// assert!(s.row_watch().is_none());
    /// ```
    pub fn set_row_watch(&mut self, watch: RowWatch) {
        if self.map.rows.as_deref().is_some_and(|j| *j.watch() == watch) {
            return;
        }
        self.map.rows = (!watch.is_empty()).then(|| Box::new(Journal::new(watch)));
    }

    /// The watch set by [`Store::set_row_watch`], if any.
    ///
    /// ```
    /// assert!(kevy_store::Store::new().row_watch().is_none());
    /// ```
    pub fn row_watch(&self) -> Option<&crate::RowWatch> {
        self.map.rows.as_deref().map(Journal::watch)
    }

    /// Whether anything was recorded since the last take.
    ///
    /// ```
    /// use kevy_store::{RowWatch, Store};
    /// let mut s = Store::new();
    /// s.set_row_watch(RowWatch::new().with_prefix("u:", Vec::new()));
    /// assert!(!s.has_row_changes());
    /// s.hset(b"u:1", &[(b"f", b"v")])?;
    /// assert!(s.has_row_changes());
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn has_row_changes(&self) -> bool {
        self.map.rows.as_deref().is_some_and(|j| !j.is_empty())
    }

    /// Everything recorded since the last take; recording starts afresh.
    /// `spare` (a record handed back from an earlier take) lends its
    /// buffers to the next record. Cold rows are read back here, so every
    /// listed row carries its fields.
    ///
    /// ```
    /// use kevy_store::{RowChanges, RowWatch, Store};
    /// let mut s = Store::new();
    /// s.set_row_watch(RowWatch::new().with_prefix("u:", vec![b"a".to_vec()]));
    /// s.hset(b"u:1", &[(b"a", b"1")])?;
    /// let first = s.take_row_changes(RowChanges::default());
    /// assert_eq!(first.len(), 1);
    /// let second = s.take_row_changes(first);
    /// assert!(second.is_empty(), "taken once");
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn take_row_changes(&mut self, spare: RowChanges) -> RowChanges {
        self.resolve_cold_rows();
        match self.map.rows.as_deref_mut() {
            Some(j) => j.take(spare),
            None => spare,
        }
    }

    /// Read back every recorded cold row while its record is still where
    /// the stub pointed. Runs before a take and before the cold tier
    /// reclaims space.
    pub(crate) fn resolve_cold_rows(&mut self) {
        #[cfg(all(feature = "std", not(target_arch = "wasm32")))]
        {
            let Some(pending) = self.map.rows.as_deref().map(Journal::pending_cold) else { return };
            for (i, key, cref) in pending {
                let v = self.tier_read_record(&key, cref);
                if let Some(j) = self.map.rows.as_deref_mut() {
                    j.resolve(i, &v);
                }
            }
        }
    }
}
