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

use std::collections::VecDeque;

/// Inline packed list storage. 24 bytes total.
#[derive(Clone)]
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

    pub fn len(&self) -> usize {
        self.count as usize
    }

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

    fn room_for(&self, elem: &[u8]) -> bool {
        elem.len() <= SMALL_LIST_ELEM_MAX
            && (self.count as usize) < SMALL_LIST_COUNT_MAX
            && (self.used as usize + 1 + elem.len()) <= SMALL_LIST_BUF_CAP
    }
}

/// Iterator over [`SmallListData`].
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
        assert_eq!(std::mem::size_of::<SmallListData>(), 24);
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
