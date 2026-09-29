//! Row pre-images for the structures derived from rows (secondary
//! indexes): the first write to a row under a watched prefix records the
//! row's watched fields as they were, before the write.
//!
//! A derived structure brought up to date when these are taken holds, for
//! each written row, exactly what the record says the row held — nothing
//! wrote the row in between — so it can find and drop the row's old entry
//! without keeping a map from key to entry of its own. Every write reaches
//! the keyspace table through [`crate::keyspace_map::Keyspace`], which
//! records before it hands out a mutable entry; that is what makes the
//! record complete whatever command, expiry, eviction or replication path
//! did the write.

use core::ops::Range;

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use crate::{Entry, Value};

/// Which rows a store watches, and which of their fields: one rule per
/// key prefix.
///
/// ```
/// use kevy_store::{RowWatch, Store};
/// let mut s = Store::new();
/// s.set_row_watch(RowWatch::new().with_prefix("user:", vec![b"age".to_vec()]));
/// s.hset(b"user:1", &[(b"age", b"41")])?;
/// let changes = s.take_row_changes(Default::default());
/// let first = changes.iter().next().expect("one row written");
/// assert_eq!((first.key(), first.field(0, 0)), (&b"user:1"[..], None), "it did not exist before");
/// # Ok::<(), kevy_store::StoreError>(())
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct RowWatch {
    rules: Vec<(Vec<u8>, Vec<Vec<u8>>)>,
}

impl RowWatch {
    /// A watch on nothing.
    ///
    /// ```
    /// assert!(kevy_store::RowWatch::new().is_empty());
    /// ```
    pub fn new() -> Self {
        Self::default()
    }

    /// This watch, also recording `fields` of every row under `prefix`.
    /// Rules are numbered in the order they are added; a row under two
    /// prefixes is recorded for both.
    ///
    /// ```
    /// use kevy_store::RowWatch;
    /// let w = RowWatch::new().with_prefix("t:", vec![b"a".to_vec(), b"b".to_vec()]);
    /// assert_eq!(w.rules(), 1);
    /// ```
    #[must_use]
    pub fn with_prefix(mut self, prefix: impl Into<Vec<u8>>, fields: Vec<Vec<u8>>) -> Self {
        self.rules.push((prefix.into(), fields));
        self
    }

    /// Whether no rule is set.
    ///
    /// ```
    /// assert!(!kevy_store::RowWatch::new().with_prefix("p:", Vec::new()).is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// How many rules.
    ///
    /// ```
    /// assert_eq!(kevy_store::RowWatch::new().rules(), 0);
    /// ```
    pub fn rules(&self) -> usize {
        self.rules.len()
    }

    /// The rules `key` falls under, one bit each.
    #[inline]
    fn mask_of(&self, key: &[u8]) -> u64 {
        let mut m = 0u64;
        for (i, (p, _)) in self.rules.iter().enumerate().take(64) {
            if key.starts_with(p) {
                m |= 1 << i;
            }
        }
        m
    }
}

/// What a row was before its first write since the last take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Was {
    /// No key.
    Absent,
    /// A key holding something other than a hash.
    NotHash,
    /// A hash; its watched fields follow.
    Hash,
    /// A cold hash whose fields the store reads before handing the
    /// changes out.
    Cold,
}

#[derive(Debug, Clone)]
struct Item {
    key: Range<u32>,
    was: Was,
    mask: u64,
    first: u32,
    #[cfg_attr(not(all(feature = "std", not(target_arch = "wasm32"))), allow(dead_code))]
    cold: Option<crate::value_cold::ColdRef>,
}

/// The rows written since the last [`crate::Store::take_row_changes`],
/// each with its watched fields as they were before the first of those
/// writes. Hand a taken value back to the next take so its buffers are
/// reused.
///
/// ```
/// use kevy_store::{RowWatch, Store};
/// let mut s = Store::new();
/// s.hset(b"u:1", &[(b"age", b"30")])?;
/// s.set_row_watch(RowWatch::new().with_prefix("u:", vec![b"age".to_vec()]));
/// s.hset(b"u:1", &[(b"age", b"31")])?;
/// s.hset(b"u:1", &[(b"age", b"32")])?;
/// let changes = s.take_row_changes(Default::default());
/// assert_eq!(changes.len(), 1, "a row is recorded once");
/// let c = changes.iter().next().expect("the row");
/// assert_eq!(c.field(0, 0), Some(&b"30"[..]), "as it was before the first write");
/// assert!(c.was_hash());
/// # Ok::<(), kevy_store::StoreError>(())
/// ```
#[derive(Debug, Clone, Default)]
pub struct RowChanges {
    reset: bool,
    bytes: Vec<u8>,
    items: Vec<Item>,
    /// Field values: a byte range, or `None` for a missing field.
    vals: Vec<Option<Range<u32>>>,
    /// Watched fields per rule.
    widths: Vec<u32>,
}

impl RowChanges {
    /// Whether the whole keyspace was wiped since the last take (FLUSH):
    /// everything derived starts over empty. The rows listed are those
    /// written after the wipe, each recorded as it was then — absent.
    ///
    /// ```
    /// use kevy_store::{RowWatch, Store};
    /// let mut s = Store::new();
    /// s.set_row_watch(RowWatch::new().with_prefix("u:", Vec::new()));
    /// s.hset(b"u:1", &[(b"f", b"v")])?;
    /// s.flushall();
    /// s.hset(b"u:2", &[(b"f", b"v")])?;
    /// let c = s.take_row_changes(Default::default());
    /// assert!(c.is_reset());
    /// let keys: Vec<&[u8]> = c.iter().map(|r| r.key()).collect();
    /// assert_eq!(keys, [&b"u:2"[..]], "only what came after the wipe");
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn is_reset(&self) -> bool {
        self.reset
    }

    /// Rows listed.
    ///
    /// ```
    /// assert_eq!(kevy_store::RowChanges::default().len(), 0);
    /// ```
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether no row is listed.
    ///
    /// ```
    /// assert!(kevy_store::RowChanges::default().is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The rows, in the order they were first written.
    ///
    /// ```
    /// use kevy_store::{RowWatch, Store};
    /// let mut s = Store::new();
    /// s.set_row_watch(RowWatch::new().with_prefix("u:", Vec::new()));
    /// s.hset(b"u:2", &[(b"f", b"v")])?;
    /// s.hset(b"u:1", &[(b"f", b"v")])?;
    /// let changes = s.take_row_changes(Default::default());
    /// let keys: Vec<&[u8]> = changes.iter().map(|c| c.key()).collect();
    /// assert_eq!(keys, [&b"u:2"[..], b"u:1"]);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn iter(&self) -> impl Iterator<Item = RowChange<'_>> {
        (0..self.items.len()).map(move |i| RowChange { c: self, i })
    }

    fn clear(&mut self) {
        self.reset = false;
        self.bytes.clear();
        self.items.clear();
        self.vals.clear();
    }
}

/// One written row, as [`RowChanges::iter`] lists it: its key, and the
/// watched fields as they were before the write.
///
/// ```
/// use kevy_store::{RowWatch, Store};
/// let mut s = Store::new();
/// s.hset(b"u:1", &[(b"age", b"30")])?;
/// s.set_row_watch(RowWatch::new().with_prefix("u:", vec![b"age".to_vec()]));
/// s.hset(b"u:1", &[(b"age", b"31")])?;
/// let changes = s.take_row_changes(Default::default());
/// let c = changes.iter().next().expect("the row was written");
/// assert_eq!(c.key(), b"u:1");
/// assert_eq!(c.field(0, 0), Some(&b"30"[..]), "the value before the write");
/// # Ok::<(), kevy_store::StoreError>(())
/// ```
#[derive(Debug, Clone, Copy)]
pub struct RowChange<'a> {
    c: &'a RowChanges,
    i: usize,
}

impl<'a> RowChange<'a> {
    fn item(&self) -> &'a Item {
        &self.c.items[self.i]
    }

    /// The row's key.
    ///
    /// ```
    /// use kevy_store::{RowWatch, Store};
    /// let mut s = Store::new();
    /// s.set_row_watch(RowWatch::new().with_prefix("u:", Vec::new()));
    /// s.del(&[&b"u:9"[..]]);
    /// s.hset(b"u:9", &[(b"f", b"v")])?;
    /// assert_eq!(s.take_row_changes(Default::default()).iter().next().map(|c| c.key().to_vec()), Some(b"u:9".to_vec()));
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn key(&self) -> &'a [u8] {
        let r = &self.item().key;
        &self.c.bytes[r.start as usize..r.end as usize]
    }

    /// Whether the row held a hash.
    ///
    /// ```
    /// use kevy_store::{RowWatch, SetCondition, Store};
    /// let mut s = Store::new();
    /// s.set(b"u:1", b"text".to_vec(), None, SetCondition::Always);
    /// s.set_row_watch(RowWatch::new().with_prefix("u:", Vec::new()));
    /// s.hset(b"u:2", &[(b"f", b"v")])?;
    /// s.del(&[&b"u:1"[..]]);
    /// let c = s.take_row_changes(Default::default());
    /// let was: Vec<bool> = c.iter().map(|c| c.was_hash()).collect();
    /// assert_eq!(was, [false, false], "u:2 did not exist, u:1 held a string");
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn was_hash(&self) -> bool {
        self.item().was == Was::Hash
    }

    /// Field `field` of rule `rule` as the row held it; `None` when the
    /// row did not fall under the rule, was not a hash, or lacked the
    /// field.
    ///
    /// ```
    /// use kevy_store::{RowWatch, Store};
    /// let mut s = Store::new();
    /// s.hset(b"u:1", &[(b"a", b"1")])?;
    /// s.set_row_watch(RowWatch::new().with_prefix("u:", vec![b"a".to_vec(), b"b".to_vec()]));
    /// s.hdel(b"u:1", &[&b"a"[..]])?;
    /// let changes = s.take_row_changes(Default::default());
    /// let c = changes.iter().next().expect("the row");
    /// assert_eq!((c.field(0, 0), c.field(0, 1), c.field(1, 0)), (Some(&b"1"[..]), None, None));
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn field(&self, rule: usize, field: usize) -> Option<&'a [u8]> {
        let it = self.item();
        if it.was != Was::Hash || rule >= 64 || it.mask & (1 << rule) == 0 {
            return None;
        }
        let before: u32 =
            (0..rule).filter(|r| it.mask & (1 << r) != 0).map(|r| self.c.widths[r]).sum();
        let r = self.c.vals.get((it.first + before) as usize + field)?.clone()?;
        Some(&self.c.bytes[r.start as usize..r.end as usize])
    }
}

/// One field of a hash value, whatever its representation.
fn field_of<'v>(v: &'v Value, f: &[u8]) -> Option<&'v [u8]> {
    match v {
        Value::Hash(h) => h.get(f).map(|b| b.as_slice()),
        Value::SegHash(h) => h.get(f).map(|b| b.as_slice()),
        Value::SmallHashInline(h) => h.get(f),
        Value::PackedRow(r) => r.get_named(f),
        _ => None,
    }
}

fn is_hash(v: &Value) -> bool {
    matches!(
        v,
        Value::Hash(_) | Value::SegHash(_) | Value::SmallHashInline(_) | Value::PackedRow(_)
    )
}

/// Recorded rows past which a lookup set replaces the linear search.
const INDEXED_PAST: usize = 16;

/// The recorder the keyspace table carries while a watch is set.
#[derive(Debug, Default)]
pub(crate) struct Journal {
    watch: RowWatch,
    changes: RowChanges,
    /// The recorded keys, once there are enough to search by hash.
    index: kevy_map::KevySet<crate::SmallBytes>,
}

impl Journal {
    pub(crate) fn new(watch: RowWatch) -> Journal {
        let widths = watch.rules.iter().map(|(_, f)| f.len() as u32).collect();
        Journal {
            watch,
            changes: RowChanges { widths, ..RowChanges::default() },
            index: Default::default(),
        }
    }

    pub(crate) fn watch(&self) -> &RowWatch {
        &self.watch
    }

    /// Record `key` as `cur` holds it, if it is watched and not yet
    /// recorded.
    #[inline]
    pub(crate) fn note(&mut self, key: &[u8], cur: Option<&Entry>) {
        let mask = self.watch.mask_of(key);
        if mask == 0 || self.holds(key) {
            return;
        }
        self.record(key, mask, cur.map(|e| &e.value));
    }

    fn holds(&mut self, key: &[u8]) -> bool {
        let c = &self.changes;
        if c.items.len() <= INDEXED_PAST {
            return c
                .items
                .iter()
                .any(|it| &c.bytes[it.key.start as usize..it.key.end as usize] == key);
        }
        if self.index.is_empty() {
            for it in &c.items {
                self.index.insert(crate::SmallBytes::from_slice(
                    &c.bytes[it.key.start as usize..it.key.end as usize],
                ));
            }
        }
        !self.index.insert(crate::SmallBytes::from_slice(key))
    }

    fn record(&mut self, key: &[u8], mask: u64, v: Option<&Value>) {
        let c = &mut self.changes;
        let start = c.bytes.len() as u32;
        c.bytes.extend_from_slice(key);
        let key_range = start..c.bytes.len() as u32;
        let (was, cold) = match v {
            None => (Was::Absent, None),
            Some(Value::Cold(r)) if r.type_tag == crate::value_cold::COLD_TAG_HASH => {
                (Was::Cold, Some(*r))
            }
            Some(v) if is_hash(v) => (Was::Hash, None),
            Some(_) => (Was::NotHash, None),
        };
        let first = c.vals.len() as u32;
        c.items.push(Item { key: key_range, was, mask, first, cold });
        if was == Was::Hash {
            let v = v.expect("a hash is a value");
            self.project(mask, v);
        }
    }

    /// Append the watched fields of every rule in `mask` from `v`.
    fn project(&mut self, mask: u64, v: &Value) {
        let c = &mut self.changes;
        for (r, (_, fields)) in self.watch.rules.iter().enumerate().take(64) {
            if mask & (1 << r) == 0 {
                continue;
            }
            for f in fields {
                let range = field_of(v, f).map(|b| {
                    let s = c.bytes.len() as u32;
                    c.bytes.extend_from_slice(b);
                    s..c.bytes.len() as u32
                });
                c.vals.push(range);
            }
        }
    }

    /// The whole keyspace went: drop what is recorded; rows written from
    /// here on are recorded as usual, from nothing.
    pub(crate) fn reset(&mut self) {
        self.index.clear();
        self.changes.clear();
        self.changes.reset = true;
    }

    /// Cold rows still to be read, as `(item, key, record)`.
    #[cfg(all(feature = "std", not(target_arch = "wasm32")))]
    pub(crate) fn pending_cold(&self) -> Vec<(usize, Vec<u8>, crate::value_cold::ColdRef)> {
        let c = &self.changes;
        c.items
            .iter()
            .enumerate()
            .filter(|(_, it)| it.was == Was::Cold)
            .map(|(i, it)| {
                (
                    i,
                    c.bytes[it.key.start as usize..it.key.end as usize].to_vec(),
                    it.cold.expect("a cold row"),
                )
            })
            .collect()
    }

    /// Item `i` (a cold row) read back as `v`.
    #[cfg(all(feature = "std", not(target_arch = "wasm32")))]
    pub(crate) fn resolve(&mut self, i: usize, v: &Value) {
        let mask = self.changes.items[i].mask;
        let first = self.changes.vals.len() as u32;
        let it = &mut self.changes.items[i];
        (it.was, it.first) = (Was::Hash, first);
        self.project(mask, v);
    }

    /// Hand the record out, taking `spare`'s buffers for the next one.
    pub(crate) fn take(&mut self, mut spare: RowChanges) -> RowChanges {
        self.index.clear();
        spare.clear();
        spare.widths.clone_from(&self.changes.widths);
        core::mem::replace(&mut self.changes, spare)
    }

    pub(crate) fn is_empty(&self) -> bool {
        !self.changes.reset && self.changes.items.is_empty()
    }
}
