//! The store's side of packed rows: converting a row, and the switch.

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;

use super::{ColumnNames, PackedRow};

/// `v`'s value for each of `names`, in their order — `None` unless `v` is
/// a hot hash in the general form and every field it holds is one of them.
///
/// A row with more fields than the table has columns is refused on its
/// length alone; otherwise one lookup per column, and a field the table
/// does not declare shows as a count short of the row's.
fn declared_columns<'a>(v: &'a crate::Value, names: &[Vec<u8>]) -> Option<Vec<Option<&'a [u8]>>> {
    use kevy_bytes::SmallBytes;
    let len = match v {
        crate::Value::Hash(h) => h.len(),
        crate::Value::SegHash(h) => h.len(),
        crate::Value::SmallHashInline(h) => h.len(),
        _ => return None,
    };
    if len > names.len() {
        return None;
    }
    let cols: Vec<Option<&[u8]>> = match v {
        crate::Value::Hash(h) => {
            names.iter().map(|n| h.get(n.as_slice()).map(SmallBytes::as_slice)).collect()
        }
        crate::Value::SegHash(h) => {
            names.iter().map(|n| h.get(n).map(SmallBytes::as_slice)).collect()
        }
        crate::Value::SmallHashInline(h) => names.iter().map(|n| h.get(n)).collect(),
        _ => return None,
    };
    (cols.iter().flatten().count() == len).then_some(cols)
}

impl crate::Store {
    /// Whether `key` currently holds the packed representation.
    ///
    /// For tests and for `MEMORY`-style introspection only. The
    /// representation is deliberately invisible on the wire; a caller that
    /// branched on it would be depending on something it must not, and the
    /// parity tests exist precisely to prove nothing needs to.
    #[doc(hidden)]
    pub fn is_packed(&mut self, key: &[u8]) -> bool {
        matches!(self.live_entry(key).map(|e| &e.value), Some(crate::Value::PackedRow(_)))
    }

    /// Whether a row under a declared prefix may take the packed form.
    ///
    /// ```
    /// use kevy_store::Store;
    /// let mut s = Store::new();
    /// assert!(!s.packed_rows_enabled());
    /// s.set_packed_rows(true);
    /// assert!(s.packed_rows_enabled());
    /// ```
    pub fn packed_rows_enabled(&self) -> bool {
        self.packed_rows
    }

    /// Allow rows under a declared prefix to take the packed representation.
    ///
    /// Off by default, and settable at runtime, so the two representations
    /// can be compared with the SAME binary — one flag apart rather than two
    /// builds apart.
    ///
    /// ```
    /// use kevy_store::Store;
    /// let mut s = Store::new();
    /// s.set_packed_rows(true);
    /// assert!(s.packed_rows_enabled());
    /// s.set_packed_rows(false);
    /// assert!(!s.packed_rows_enabled());
    /// ```
    pub fn set_packed_rows(&mut self, on: bool) {
        self.packed_rows = on;
    }

    /// Whether `key` holds a hot hash in the general form. A row already
    /// packed (every write after a row's first) and a cold row both answer
    /// no: a cold row holds no memory to save, and reading it would be a
    /// disk read nobody asked for — and a first touch, after which the
    /// client's own first read would promote it.
    fn hot_general_hash(&mut self, key: &[u8]) -> bool {
        matches!(
            self.live_entry(key).map(|e| &e.value),
            Some(
                crate::Value::Hash(_) | crate::Value::SmallHashInline(_) | crate::Value::SegHash(_)
            )
        )
    }
    /// Convert `key`'s hash into the packed form for a table declaring
    /// `names`, if it is a hash that is not packed already.
    ///
    /// A value the row holds under a name the table does not declare would be
    /// lost, so its presence refuses the conversion outright and the row keeps
    /// the general form. Nothing here may drop a value. A cold row is left
    /// cold and unread: it holds no memory for the packed form to save. Its
    /// table's names are kept instead, and the row is packed on them when a
    /// read promotes it, if it fits.
    ///
    /// `names` is the table's own list: the row points at it rather than at
    /// a copy, so pass the same list for every row of a table.
    ///
    /// ```
    /// use kevy_store::Store;
    /// use kevy_store::packed_row::ColumnNames;
    /// let mut s = Store::new();
    /// s.hset(b"user:1", &[(b"id".as_slice(), b"7".as_slice()), (b"dept", b"eng")])?;
    /// let table: ColumnNames = vec![b"id".to_vec(), b"name".to_vec(), b"dept".to_vec()].into();
    /// s.pack_row(b"user:1", &table);
    /// // the representation changed; what the key answers did not
    /// assert_eq!(s.hget(b"user:1", b"dept")?, Some(&b"eng"[..]));
    /// assert_eq!(s.hlen(b"user:1")?, 2);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn pack_row(&mut self, key: &[u8], names: &ColumnNames) {
        // a field whose own TTL has passed must not be packed; purging it
        // can promote, so only a hot row is purged
        if !self.hfttl.is_empty() {
            if !self.hot_general_hash(key) {
                if matches!(self.live_entry(key).map(|e| &e.value), Some(crate::Value::Cold(_))) {
                    self.share_shape(names);
                }
                return;
            }
            self.purge_hash_ttl(key);
        }
        let Some(e) = self.live_entry(key) else { return };
        let cold = matches!(e.value, crate::Value::Cold(_));
        let Some(row) = declared_columns(&e.value, names).and_then(|c| PackedRow::build(names, &c))
        else {
            if cold {
                // it stays cold, and is packed on these names when promoted
                self.share_shape(names);
            }
            return;
        };
        self.share_shape(names);
        if let Some(e) = self.live_entry_mut(key) {
            e.value = crate::Value::PackedRow(row);
        }
        self.reweigh_entry(key);
    }

    /// Keep `names` among the shapes a row from the cold tier is rebuilt
    /// on. A shape nothing but this list still holds belongs to no table
    /// and no row any more, so it goes when a new one arrives.
    pub(crate) fn share_shape(&mut self, names: &ColumnNames) {
        if self.row_shapes.iter().any(|s| alloc::sync::Arc::ptr_eq(s, names)) {
            return;
        }
        self.row_shapes.retain(|s| alloc::sync::Arc::strong_count(s) > 1);
        self.row_shapes.push(names.clone());
    }
}
