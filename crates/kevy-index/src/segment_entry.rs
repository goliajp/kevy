//! One index row, allocated once and shared by both directions of a
//! [`crate::Segment`]: the ordered tree holds it as [`ByValue`], the
//! reverse set as [`ByKey`]. Each side is a pointer, so a row's value and
//! key are stored once however many ways it is reached.

use std::borrow::Borrow;
use std::cmp::Ordering;
use std::hash::{Hash, Hasher};
use std::mem::size_of;
use std::sync::Arc;

use crate::value::IndexValue;

type Row = (IndexValue, Box<[u8]>);

/// Heap bytes one row costs: the shared allocation (two reference
/// counts in front of the row) plus the key and any string value.
pub(crate) fn row_bytes(v: &IndexValue, key: &[u8]) -> u64 {
    let value_heap = match v {
        IndexValue::Str(s) => s.capacity(),
        IndexValue::I64(_) | IndexValue::F64(_) => 0,
    };
    (2 * size_of::<usize>() + size_of::<Row>() + key.len() + value_heap) as u64
}

/// Anything that reads as a `(value, key)` pair, so the tree can be
/// searched with borrowed parts instead of a built row.
pub(crate) trait RowRef {
    fn value(&self) -> &IndexValue;
    fn key(&self) -> &[u8];
}

impl RowRef for Row {
    fn value(&self) -> &IndexValue {
        &self.0
    }
    fn key(&self) -> &[u8] {
        &self.1
    }
}

impl RowRef for (&IndexValue, &[u8]) {
    fn value(&self) -> &IndexValue {
        self.0
    }
    fn key(&self) -> &[u8] {
        self.1
    }
}

impl PartialEq for dyn RowRef + '_ {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for dyn RowRef + '_ {}

impl PartialOrd for dyn RowRef + '_ {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for dyn RowRef + '_ {
    fn cmp(&self, other: &Self) -> Ordering {
        self.value().cmp(other.value()).then_with(|| self.key().cmp(other.key()))
    }
}

/// A row in `(value, key)` order.
#[derive(Debug)]
pub(crate) struct ByValue(Arc<Row>);

/// The same row, hashed and compared by key alone. Its `Hash` must stay
/// exactly `[u8]`'s so that `&[u8]` lookups land on it.
#[derive(Debug)]
pub(crate) struct ByKey(Arc<Row>);

/// A new row and its two handles.
pub(crate) fn share(v: IndexValue, key: &[u8]) -> (ByValue, ByKey) {
    let row = Arc::new((v, Box::from(key)));
    (ByValue(Arc::clone(&row)), ByKey(row))
}

impl ByValue {
    pub(crate) fn pair(&self) -> (&IndexValue, &[u8]) {
        (&self.0.0, &self.0.1)
    }

    /// The row as owned parts; the reverse handle must already be gone,
    /// or the parts are copied.
    pub(crate) fn into_parts(self) -> (IndexValue, Vec<u8>) {
        let (v, k) = Arc::unwrap_or_clone(self.0);
        (v, k.into_vec())
    }
}

impl RowRef for ByKey {
    fn value(&self) -> &IndexValue {
        &self.0.0
    }
    fn key(&self) -> &[u8] {
        &self.0.1
    }
}

impl RowRef for ByValue {
    fn value(&self) -> &IndexValue {
        &self.0.0
    }
    fn key(&self) -> &[u8] {
        &self.0.1
    }
}

impl<'a> Borrow<dyn RowRef + 'a> for ByValue {
    fn borrow(&self) -> &(dyn RowRef + 'a) {
        self
    }
}

impl PartialEq for ByValue {
    fn eq(&self, other: &Self) -> bool {
        (self as &dyn RowRef) == (other as &dyn RowRef)
    }
}

impl Eq for ByValue {}

impl PartialOrd for ByValue {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ByValue {
    fn cmp(&self, other: &Self) -> Ordering {
        (self as &dyn RowRef).cmp(other as &dyn RowRef)
    }
}

impl PartialEq for ByKey {
    fn eq(&self, other: &Self) -> bool {
        self.0.1 == other.0.1
    }
}

impl Eq for ByKey {}

impl Hash for ByKey {
    fn hash<H: Hasher>(&self, h: &mut H) {
        self.0.1.hash(h);
    }
}

impl Borrow<[u8]> for ByKey {
    fn borrow(&self) -> &[u8] {
        &self.0.1
    }
}
