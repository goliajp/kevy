//! `SmallListData` — inline-listpack encoding for tiny lists.
//!
//! Companion to [`crate::small_set::SmallSetData`]. Mirrors valkey's
//! `OBJ_ENCODING_LISTPACK` for lists (`t_list.c::listTypeTryConversion`).
//! For the `redis-benchmark -t lpush/-t rpush` default shape (single
//! literal value `__rand_int__`), a one-element list fits in one cache
//! line. The encoding-switch promotes to `Value::List(Arc<VecDeque>)`
//! on overflow.
//!
//! ## Layout — 24 bytes packed
//!
//! Same shape as [`crate::small_set::SmallSetData`]:
//!
//! ```text
//! offset: 0    1                                 23
//!         +----+----+----+----+----+ ...     +-----+
//!         | n  | u  |       buf[22]              |
//!         +----+----+----+----+----+ ...     +-----+
//! ```
//!
//! - `n` (u8): element count (`0..=COUNT_MAX`).
//! - `u` (u8): bytes used (sum of `1 + len_i`).
//! - `buf` ([u8; 22]): packed `[len_i: u8][elem_i: u8; len_i]` entries.
//!
//! Unlike sets, lists allow duplicates and preserve order. LPUSH
//! prepends (entries are shifted right to make room); RPUSH appends.

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use alloc::collections::VecDeque;

/// Inline packed list storage. 24 bytes total.
///
/// ```
/// use kevy_store::{Store, Value};
/// let mut s = Store::new();
/// s.rpush(b"l", &[b"a".as_slice(), b"b"])?;
/// s.snapshot_each(|_, v, _| {
///     let Value::SmallListInline(l) = v else { panic!("a two-item list stays inline") };
///     assert_eq!(l.len(), 2);
/// });
/// # Ok::<(), kevy_store::StoreError>(())
/// ```
#[derive(Debug, Clone)]
pub struct SmallListData {
    count: u8,
    used: u8,
    buf: [u8; SMALL_LIST_BUF_CAP],
}

pub(crate) const SMALL_LIST_BUF_CAP: usize = 22;
pub(crate) const SMALL_LIST_ELEM_MAX: usize = SMALL_LIST_BUF_CAP - 1;
pub(crate) const SMALL_LIST_COUNT_MAX: usize = 8;

/// Outcome of `try_push_*`.
pub(crate) enum PushResult {
    Pushed,
    NoRoom,
}

impl SmallListData {
    pub(crate) fn new() -> Self {
        Self { count: 0, used: 0, buf: [0; SMALL_LIST_BUF_CAP] }
    }

    pub(crate) fn with_one(elem: &[u8]) -> Option<Self> {
        if elem.len() > SMALL_LIST_ELEM_MAX {
            return None;
        }
        let mut s = Self::new();
        s.buf[0] = elem.len() as u8;
        s.buf[1..1 + elem.len()].copy_from_slice(elem);
        s.count = 1;
        s.used = 1 + elem.len() as u8;
        Some(s)
    }

    /// Element count, held rather than derived — the flat encoding would
    /// have to be walked to count.
    pub fn len(&self) -> usize {
        self.count as usize
    }

    /// Whether the list holds no elements.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Iterator yielding elements as `&[u8]` (front → back).
    pub fn iter(&self) -> SmallListIter<'_> {
        SmallListIter { buf: &self.buf[..self.used as usize], cursor: 0 }
    }

    /// Append at the back (RPUSH).
    pub(crate) fn try_push_back(&mut self, elem: &[u8]) -> PushResult {
        if !self.room_for(elem) {
            return PushResult::NoRoom;
        }
        let need = 1 + elem.len();
        let off = self.used as usize;
        self.buf[off] = elem.len() as u8;
        self.buf[off + 1..off + need].copy_from_slice(elem);
        self.used += need as u8;
        self.count += 1;
        PushResult::Pushed
    }

    /// Prepend at the front (LPUSH).
    pub(crate) fn try_push_front(&mut self, elem: &[u8]) -> PushResult {
        if !self.room_for(elem) {
            return PushResult::NoRoom;
        }
        let need = 1 + elem.len();
        let used = self.used as usize;
        // Shift everything right by `need` bytes.
        self.buf.copy_within(0..used, need);
        self.buf[0] = elem.len() as u8;
        self.buf[1..need].copy_from_slice(elem);
        self.used += need as u8;
        self.count += 1;
        PushResult::Pushed
    }

    /// Take the first (`front`) or the last element, handed to `f` just
    /// before it goes; `false` when there is none.
    pub(crate) fn pop_with(&mut self, front: bool, f: impl FnOnce(&[u8])) -> bool {
        if self.count == 0 {
            return false;
        }
        let used = self.used as usize;
        let mut at = 0;
        if !front {
            for _ in 1..self.count {
                at += 1 + self.buf[at] as usize;
            }
        }
        let len = self.buf[at] as usize;
        f(&self.buf[at + 1..at + 1 + len]);
        if front {
            self.buf.copy_within(1 + len..used, 0);
        }
        self.used = (used - 1 - len) as u8;
        self.count -= 1;
        true
    }

    fn room_for(&self, elem: &[u8]) -> bool {
        elem.len() <= SMALL_LIST_ELEM_MAX
            && (self.count as usize) < SMALL_LIST_COUNT_MAX
            && (self.used as usize + 1 + elem.len()) <= SMALL_LIST_BUF_CAP
    }
}

/// Iterator over [`SmallListData`].
///
/// ```
/// use kevy_store::{Store, Value};
/// let mut s = Store::new();
/// s.rpush(b"l", &[b"a".as_slice(), b"b"])?;
/// s.lpush(b"l", &[b"z".as_slice()])?;
/// let mut items = Vec::new();
/// s.snapshot_each(|_, v, _| {
///     if let Value::SmallListInline(l) = v {
///         items = l.iter().map(<[u8]>::to_vec).collect();
///     }
/// });
/// assert_eq!(items, [b"z".to_vec(), b"a".to_vec(), b"b".to_vec()], "head to tail");
/// # Ok::<(), kevy_store::StoreError>(())
/// ```
#[derive(Debug)]
pub struct SmallListIter<'a> {
    buf: &'a [u8],
    cursor: usize,
}

impl<'a> Iterator for SmallListIter<'a> {
    type Item = &'a [u8];
    fn next(&mut self) -> Option<&'a [u8]> {
        if self.cursor >= self.buf.len() {
            return None;
        }
        let len = self.buf[self.cursor] as usize;
        let start = self.cursor + 1;
        let end = start + len;
        self.cursor = end;
        Some(&self.buf[start..end])
    }
}

/// Materialise the inline list as a heap-backed [`crate::value::ListData`].
pub(crate) fn promote(inline: &SmallListData) -> crate::value::ListData {
    let mut d: VecDeque<Vec<u8>> = VecDeque::with_capacity(inline.len().max(1));
    for e in inline.iter() {
        d.push_back(e.to_vec());
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_is_24_bytes() {
        assert_eq!(core::mem::size_of::<SmallListData>(), 24);
    }

    #[test]
    fn push_back_basic() {
        let mut l = SmallListData::new();
        assert!(matches!(l.try_push_back(b"a"), PushResult::Pushed));
        assert!(matches!(l.try_push_back(b"bb"), PushResult::Pushed));
        let v: Vec<&[u8]> = l.iter().collect();
        assert_eq!(v, vec![b"a".as_slice(), b"bb".as_slice()]);
        assert_eq!(l.len(), 2);
    }

    #[test]
    fn push_front_basic() {
        let mut l = SmallListData::new();
        assert!(matches!(l.try_push_front(b"a"), PushResult::Pushed));
        assert!(matches!(l.try_push_front(b"bb"), PushResult::Pushed));
        let v: Vec<&[u8]> = l.iter().collect();
        assert_eq!(v, vec![b"bb".as_slice(), b"a".as_slice()]);
    }

    #[test]
    fn duplicate_allowed() {
        let mut l = SmallListData::new();
        l.try_push_back(b"a");
        l.try_push_back(b"a");
        assert_eq!(l.len(), 2);
        let v: Vec<&[u8]> = l.iter().collect();
        assert_eq!(v, vec![b"a".as_slice(), b"a".as_slice()]);
    }

    #[test]
    fn no_room_when_full() {
        let mut l = SmallListData::new();
        let big = b"element:__rand_int__"; // 20 bytes
        assert_eq!(big.len(), 20);
        assert!(matches!(l.try_push_back(big), PushResult::Pushed));
        // Used = 21 of 22, second 20-byte element won't fit.
        assert!(matches!(l.try_push_back(big), PushResult::NoRoom));
    }

    #[test]
    fn elem_too_long() {
        let mut l = SmallListData::new();
        let big = vec![b'x'; SMALL_LIST_ELEM_MAX + 1];
        assert!(matches!(l.try_push_back(&big), PushResult::NoRoom));
    }

    #[test]
    fn promote_preserves_order() {
        let mut l = SmallListData::new();
        l.try_push_back(b"a");
        l.try_push_back(b"bb");
        l.try_push_back(b"ccc");
        let d = promote(&l);
        let v: Vec<&Vec<u8>> = d.iter().collect();
        assert_eq!(v[0], b"a");
        assert_eq!(v[1], b"bb");
        assert_eq!(v[2], b"ccc");
    }
}

#[cfg(test)]
mod tests_pop {
    use super::{PushResult, SmallListData};
    use alloc::collections::VecDeque;
    use alloc::vec::Vec;

    /// Pushes and pops at both ends against a deque, element lengths from
    /// empty to the most one slot takes.
    #[test]
    fn pops_at_both_ends_match_a_deque() {
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let mut l = SmallListData::new();
        let mut model: VecDeque<Vec<u8>> = VecDeque::new();
        for i in 0..20_000u32 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let front = x & 1 == 0;
            if x & 6 == 0 || model.is_empty() {
                let e: Vec<u8> =
                    (0..(x >> 8) % 12).map(|j| (i as u8).wrapping_add(j as u8)).collect();
                let pushed = if front { l.try_push_front(&e) } else { l.try_push_back(&e) };
                if matches!(pushed, PushResult::Pushed) {
                    if front { model.push_front(e) } else { model.push_back(e) }
                }
            } else {
                let mut got = None;
                assert!(l.pop_with(front, |e| got = Some(e.to_vec())));
                let want = if front { model.pop_front() } else { model.pop_back() };
                assert_eq!(got, want);
            }
            assert_eq!(l.len(), model.len());
            assert!(l.iter().eq(model.iter().map(Vec::as_slice)));
        }
        while l.pop_with(true, |_| {}) {}
        assert!(!l.pop_with(false, |_| {}), "nothing left to pop");
    }
}
