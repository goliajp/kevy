//! One index row, allocated once and shared by both directions of a
//! [`crate::Segment`]: the ordered tree holds it as [`ByValue`], the
//! reverse set as [`ByKey`]. Each side is a pointer, so a row's value and
//! key are stored once however many ways it is reached.
//!
//! The row is one allocation — a reference count, the key's length and the
//! value, with the key's bytes right behind them — so a key of any length
//! costs its bytes and no allocation of its own.

use std::alloc::{self, Layout};
use std::borrow::Borrow;
use std::cmp::Ordering;
use std::hash::{Hash, Hasher};
use std::ptr::NonNull;
use std::sync::atomic::{self, AtomicU32};

use crate::value::IndexValue;

#[repr(C)]
struct Header {
    refs: AtomicU32,
    key_len: u32,
    value: IndexValue,
}

/// A counted handle to one row; two exist while the row is indexed.
struct Row(NonNull<Header>);

// SAFETY: a row is never written after it is built except through its
// atomic count, and its value is `Send + Sync`, so handles may move and be
// shared across threads exactly as `Arc<(IndexValue, [u8])>` could.
unsafe impl Send for Row {}
// SAFETY: see the `Send` impl above.
unsafe impl Sync for Row {}

impl Row {
    fn layout(key_len: usize) -> Layout {
        let key = Layout::array::<u8>(key_len).expect("a key under isize::MAX");
        let (layout, _) = Layout::new::<Header>().extend(key).expect("a row under isize::MAX");
        layout.pad_to_align()
    }

    fn new(value: IndexValue, key: &[u8]) -> Row {
        let key_len = u32::try_from(key.len()).expect("a key under 4 GiB");
        let layout = Row::layout(key.len());
        // SAFETY: the layout is non-zero-sized — it holds a `Header`.
        let raw = unsafe { alloc::alloc(layout) }.cast::<Header>();
        let Some(ptr) = NonNull::new(raw) else { alloc::handle_alloc_error(layout) };
        // SAFETY: `ptr` is a fresh allocation sized and aligned for a
        // `Header` followed by `key.len()` bytes; both writes stay inside it
        // and nothing reads it before they are done.
        unsafe {
            ptr.as_ptr().write(Header { refs: AtomicU32::new(1), key_len, value });
            let tail = ptr.as_ptr().cast::<u8>().add(size_of::<Header>());
            std::ptr::copy_nonoverlapping(key.as_ptr(), tail, key.len());
        }
        Row(ptr)
    }

    fn header(&self) -> &Header {
        // SAFETY: the pointer came from `new` and the row lives until the
        // last handle drops, which is not before this borrow of a handle ends.
        unsafe { self.0.as_ref() }
    }

    fn value(&self) -> &IndexValue {
        &self.header().value
    }

    fn key(&self) -> &[u8] {
        let len = self.header().key_len as usize;
        // SAFETY: `new` copied exactly `key_len` bytes right after the header,
        // in the same allocation, and they are never written again.
        unsafe {
            std::slice::from_raw_parts(self.0.as_ptr().cast::<u8>().add(size_of::<Header>()), len)
        }
    }

    fn share(&self) -> Row {
        // a new handle needs no ordering: it is made from one already held
        self.header().refs.fetch_add(1, atomic::Ordering::Relaxed);
        Row(self.0)
    }

    /// The value and the key, moved out when this is the last handle.
    fn into_parts(self) -> (IndexValue, Vec<u8>) {
        if self.header().refs.load(atomic::Ordering::Acquire) != 1 {
            return (self.value().clone(), self.key().to_vec());
        }
        let key = self.key().to_vec();
        let layout = Row::layout(key.len());
        let ptr = self.0.as_ptr();
        std::mem::forget(self);
        // SAFETY: this handle was the only one, so nothing else reads the
        // row; the value is read out once and the allocation freed without
        // dropping it again, with the layout it was made with.
        unsafe {
            let value = std::ptr::read(&raw const (*ptr).value);
            alloc::dealloc(ptr.cast::<u8>(), layout);
            (value, key)
        }
    }
}

impl Drop for Row {
    fn drop(&mut self) {
        if self.header().refs.fetch_sub(1, atomic::Ordering::Release) != 1 {
            return;
        }
        // every other handle's last use happens before the row goes
        atomic::fence(atomic::Ordering::Acquire);
        let layout = Row::layout(self.header().key_len as usize);
        let ptr = self.0.as_ptr();
        // SAFETY: the count reached zero, so this is the last handle and no
        // other reads or drops the row; the value is dropped once and the
        // allocation freed with the layout `new` made it with.
        unsafe {
            std::ptr::drop_in_place(&raw mut (*ptr).value);
            alloc::dealloc(ptr.cast::<u8>(), layout);
        }
    }
}

impl std::fmt::Debug for Row {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Row").field(self.value()).field(&self.key()).finish()
    }
}

/// Buckets std's hash table allocates to hold `rows`: a power of two it
/// fills to at most 7/8, with 4 and 8 as the small sizes.
pub(crate) fn table_buckets(rows: usize) -> usize {
    match rows {
        0 => 0,
        1..=3 => 4,
        4..=7 => 8,
        _ => (rows * 8 / 7).next_power_of_two(),
    }
}

/// Heap bytes one row costs: the one allocation holding its count, value
/// and key, plus any string value's own.
pub(crate) fn row_bytes(v: &IndexValue, key: &[u8]) -> u64 {
    let value_heap = match v {
        IndexValue::Str(s) => s.capacity(),
        IndexValue::I64(_) | IndexValue::F64(_) => 0,
    };
    (Row::layout(key.len()).size() + value_heap) as u64
}

/// Anything that reads as a `(value, key)` pair, so the tree can be
/// searched with borrowed parts instead of a built row.
pub(crate) trait RowRef {
    fn value(&self) -> &IndexValue;
    fn key(&self) -> &[u8];
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
pub(crate) struct ByValue(Row);

/// The same row, hashed and compared by key alone. Its `Hash` must stay
/// exactly `[u8]`'s so that `&[u8]` lookups land on it.
#[derive(Debug)]
pub(crate) struct ByKey(Row);

/// A new row and its two handles.
pub(crate) fn share(v: IndexValue, key: &[u8]) -> (ByValue, ByKey) {
    let row = Row::new(v, key);
    (ByValue(row.share()), ByKey(row))
}

impl ByValue {
    pub(crate) fn pair(&self) -> (&IndexValue, &[u8]) {
        (self.0.value(), self.0.key())
    }

    /// The row as owned parts; the reverse handle must already be gone,
    /// or the parts are copied.
    pub(crate) fn into_parts(self) -> (IndexValue, Vec<u8>) {
        self.0.into_parts()
    }
}

impl RowRef for ByKey {
    fn value(&self) -> &IndexValue {
        self.0.value()
    }
    fn key(&self) -> &[u8] {
        self.0.key()
    }
}

impl RowRef for ByValue {
    fn value(&self) -> &IndexValue {
        self.0.value()
    }
    fn key(&self) -> &[u8] {
        self.0.key()
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
        self.0.key() == other.0.key()
    }
}

impl Eq for ByKey {}

impl Hash for ByKey {
    fn hash<H: Hasher>(&self, h: &mut H) {
        self.0.key().hash(h);
    }
}

impl Borrow<[u8]> for ByKey {
    fn borrow(&self) -> &[u8] {
        self.0.key()
    }
}

#[cfg(test)]
mod row_tests {
    use super::*;

    #[test]
    fn a_row_is_its_header_and_its_key() {
        assert_eq!(size_of::<Header>(), 32);
        let (v, k) = share(IndexValue::Str(b"tokyo".to_vec()), b"user:42");
        assert_eq!(v.pair(), (&IndexValue::Str(b"tokyo".to_vec()), &b"user:42"[..]));
        assert_eq!(<ByKey as Borrow<[u8]>>::borrow(&k), b"user:42");
        drop(k);
        assert_eq!(v.into_parts(), (IndexValue::Str(b"tokyo".to_vec()), b"user:42".to_vec()));
    }

    #[test]
    fn parts_are_copied_while_the_other_handle_lives() {
        let (v, k) = share(IndexValue::I64(-7), &[0u8; 300]);
        assert_eq!(v.into_parts(), (IndexValue::I64(-7), vec![0u8; 300]));
        assert_eq!((k.value(), k.key().len()), (&IndexValue::I64(-7), 300));
        let (v, k) = share(IndexValue::F64(1.5), b"");
        drop(v);
        assert_eq!(k.key(), b"");
    }

    #[test]
    fn handles_cross_threads() {
        let rows: Vec<(ByValue, ByKey)> =
            (0..64).map(|i| share(IndexValue::I64(i), format!("k{i}").as_bytes())).collect();
        let (vs, ks): (Vec<ByValue>, Vec<ByKey>) = rows.into_iter().unzip();
        let t = std::thread::spawn(move || ks.iter().map(|k| k.key().len()).sum::<usize>());
        let sum: i64 = vs
            .iter()
            .map(|v| match v.value() {
                IndexValue::I64(n) => *n,
                _ => 0,
            })
            .sum();
        assert_eq!((t.join().unwrap(), sum), (64 * 2 + 54, (0..64).sum()));
    }
}

#[cfg(test)]
mod bucket_tests {
    use super::table_buckets;

    // the model against std itself: with no removals, a table's reported
    // capacity is exactly what its bucket count allows
    #[test]
    fn table_buckets_matches_std_growth() {
        for rows in 0..300usize {
            // one insert at a time, the way an index grows
            let mut set = std::collections::HashSet::new();
            for r in 0..rows {
                set.insert(r);
            }
            let b = table_buckets(rows);
            let cap = if b < 8 { b.saturating_sub(1) } else { b / 8 * 7 };
            assert_eq!(set.capacity(), cap, "{rows} rows");
        }
    }
}
