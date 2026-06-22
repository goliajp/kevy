//! `SmallHashData` — inline-listpack encoding for tiny hashes.
//!
//! Companion to [`crate::small_set::SmallSetData`] (v1.25 A.7 pilot).
//! Mirrors valkey's `OBJ_ENCODING_LISTPACK` for hashes (`t_hash.c::
//! hashTypeTryConversion`). For the `redis-benchmark -t hset` default
//! shape (single field `field:__rand_int__` ~24 B + literal value
//! `__rand_int__`), the structural prerequisite is "one (field, value)
//! tuple in one cache line" — same lever as small_set.
//!
//! ## Layout — 24 bytes packed
//!
//! ```text
//! offset: 0    1                                 23
//!         +----+----+----+----+----+ ...     +-----+
//!         | n  | u  |       buf[22]              |
//!         +----+----+----+----+----+ ...     +-----+
//! ```
//!
//! - `n` (u8): tuple count (`0..=COUNT_MAX`).
//! - `u` (u8): bytes used in `buf` (sum of `2 + flen_i + vlen_i`).
//! - `buf` ([u8; 22]): packed `[flen][field][vlen][value]` tuples.
//!
//! Two length prefix bytes per tuple → field + value share the 20-byte
//! payload budget after the two prefix bytes. A 6-byte `field` +
//! 12-byte value fits with 2 bytes spare. The redis-benchmark default
//! `field` literal is `field:__rand_int__` (= 20 bytes); already
//! overflows the inline buffer with the prefix overhead. The win is
//! the **single very short field-value pair** workload — small struct-
//! field hashes like `{"name": "alice"}`, which valkey's listpack also
//! catches.
//!
//! ## Upgrade trigger
//!
//! `try_set` returns [`AddResult::NoRoom`] when (a) field len > cap, (b)
//! value len > cap, (c) replacing-an-existing-field would overflow, or
//! (d) appending a new pair would overflow the 22-byte budget. Caller
//! upgrades to `Value::Hash(Arc<HashData>)` and re-inserts.

use kevy_bytes::SmallBytes;

/// Inline packed hash storage. 24 bytes total.
#[derive(Clone)]
pub struct SmallHashData {
    count: u8,
    used: u8,
    buf: [u8; SMALL_HASH_BUF_CAP],
}

pub(crate) const SMALL_HASH_BUF_CAP: usize = 22;

/// Per-field name cap. With one prefix byte the field can be at most
/// `BUF_CAP - 3` bytes (leave 1 byte for value length and 1 byte for
/// at least a 0-length value — caller may still NoRoom on value).
pub(crate) const SMALL_HASH_FIELD_MAX: usize = SMALL_HASH_BUF_CAP - 3;

/// Per-value cap mirroring the field cap.
pub(crate) const SMALL_HASH_VALUE_MAX: usize = SMALL_HASH_BUF_CAP - 3;

/// Hard count cap (defensive; byte budget usually bites first).
pub(crate) const SMALL_HASH_COUNT_MAX: usize = 8;

/// Outcome of [`SmallHashData::try_set`].
pub(crate) enum AddResult {
    /// Field was new; count + used updated.
    Added,
    /// Field existed; value was updated in place (or, if the new value
    /// fits with no shift, written over the old one).
    Updated,
    /// Pair doesn't fit (oversized field/value or buffer full).
    NoRoom,
}

impl SmallHashData {
    pub(crate) fn new() -> Self {
        Self { count: 0, used: 0, buf: [0; SMALL_HASH_BUF_CAP] }
    }

    /// Construct holding one `(field, value)` pair if it fits.
    pub(crate) fn with_one(field: &[u8], value: &[u8]) -> Option<Self> {
        if field.len() > SMALL_HASH_FIELD_MAX || value.len() > SMALL_HASH_VALUE_MAX {
            return None;
        }
        let need = 2 + field.len() + value.len();
        if need > SMALL_HASH_BUF_CAP {
            return None;
        }
        let mut s = Self::new();
        s.write_pair_at(0, field, value);
        s.count = 1;
        s.used = need as u8;
        Some(s)
    }

    pub fn len(&self) -> usize {
        self.count as usize
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Look up `field`; returns the value bytes if present.
    pub fn get(&self, field: &[u8]) -> Option<&[u8]> {
        for (f, v) in self.iter() {
            if f == field {
                return Some(v);
            }
        }
        None
    }

    /// Does this small hash contain `field`?
    pub fn contains_key(&self, field: &[u8]) -> bool {
        self.get(field).is_some()
    }

    pub fn iter(&self) -> SmallHashIter<'_> {
        SmallHashIter { buf: &self.buf[..self.used as usize], cursor: 0 }
    }

    /// Try to set `field -> value`. See [`AddResult`].
    pub(crate) fn try_set(&mut self, field: &[u8], value: &[u8]) -> AddResult {
        if field.len() > SMALL_HASH_FIELD_MAX || value.len() > SMALL_HASH_VALUE_MAX {
            return AddResult::NoRoom;
        }
        // Existing field? Replace value (may need shift).
        if let Some((off, flen, old_vlen)) = self.locate(field) {
            let new_vlen = value.len();
            let used = self.used as usize;
            let val_off = off + 1 + flen + 1;
            let old_end = val_off + old_vlen;
            let delta: isize = new_vlen as isize - old_vlen as isize;
            let new_used_i = used as isize + delta;
            if new_used_i > SMALL_HASH_BUF_CAP as isize {
                return AddResult::NoRoom;
            }
            let new_used = new_used_i as usize;
            // Shift the tail to make room (or close the gap).
            if delta != 0 {
                self.buf.copy_within(old_end..used, (val_off + new_vlen) as usize);
                if delta < 0 {
                    // Zero freed tail.
                    self.buf[new_used..used].fill(0);
                }
            }
            self.buf[val_off - 1] = new_vlen as u8;
            self.buf[val_off..val_off + new_vlen].copy_from_slice(value);
            self.used = new_used as u8;
            return AddResult::Updated;
        }
        // New field — append.
        if self.count as usize >= SMALL_HASH_COUNT_MAX {
            return AddResult::NoRoom;
        }
        let need = 2 + field.len() + value.len();
        let new_used = self.used as usize + need;
        if new_used > SMALL_HASH_BUF_CAP {
            return AddResult::NoRoom;
        }
        let off = self.used as usize;
        self.write_pair_at(off, field, value);
        self.used = new_used as u8;
        self.count += 1;
        AddResult::Added
    }

    /// Try to remove `field`. Returns whether it was present.
    pub(crate) fn try_remove(&mut self, field: &[u8]) -> bool {
        let Some((off, flen, vlen)) = self.locate(field) else {
            return false;
        };
        let used = self.used as usize;
        let entry_end = off + 2 + flen + vlen;
        self.buf.copy_within(entry_end..used, off);
        let shifted = used - entry_end;
        let new_used = off + shifted;
        self.buf[new_used..used].fill(0);
        self.used = new_used as u8;
        self.count -= 1;
        true
    }

    fn write_pair_at(&mut self, off: usize, field: &[u8], value: &[u8]) {
        self.buf[off] = field.len() as u8;
        let fstart = off + 1;
        let fend = fstart + field.len();
        self.buf[fstart..fend].copy_from_slice(field);
        self.buf[fend] = value.len() as u8;
        let vstart = fend + 1;
        let vend = vstart + value.len();
        self.buf[vstart..vend].copy_from_slice(value);
    }

    /// Returns (entry_offset, field_len, value_len) if `field` is present.
    fn locate(&self, field: &[u8]) -> Option<(usize, usize, usize)> {
        let mut cursor = 0usize;
        let used = self.used as usize;
        while cursor < used {
            let flen = self.buf[cursor] as usize;
            let fstart = cursor + 1;
            let fend = fstart + flen;
            let vlen = self.buf[fend] as usize;
            if &self.buf[fstart..fend] == field {
                return Some((cursor, flen, vlen));
            }
            cursor = fend + 1 + vlen;
        }
        None
    }
}

/// Iterator over [`SmallHashData`] yielding `(&[u8] field, &[u8] value)`.
pub struct SmallHashIter<'a> {
    buf: &'a [u8],
    cursor: usize,
}

impl<'a> Iterator for SmallHashIter<'a> {
    type Item = (&'a [u8], &'a [u8]);
    fn next(&mut self) -> Option<Self::Item> {
        if self.cursor >= self.buf.len() {
            return None;
        }
        let flen = self.buf[self.cursor] as usize;
        let fstart = self.cursor + 1;
        let fend = fstart + flen;
        let vlen = self.buf[fend] as usize;
        let vstart = fend + 1;
        let vend = vstart + vlen;
        self.cursor = vend;
        Some((&self.buf[fstart..fend], &self.buf[vstart..vend]))
    }
}

/// Materialise the inline hash as a heap-backed [`crate::value::HashData`].
pub(crate) fn promote(inline: &SmallHashData) -> crate::value::HashData {
    let mut h = crate::value::HashData::with_capacity(inline.len().max(1));
    for (f, v) in inline.iter() {
        h.insert(SmallBytes::from_slice(f), v.to_vec());
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_is_24_bytes() {
        assert_eq!(std::mem::size_of::<SmallHashData>(), 24);
    }

    #[test]
    fn with_one_and_get() {
        let s = SmallHashData::with_one(b"name", b"alice").unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s.get(b"name"), Some(b"alice".as_slice()));
        assert!(s.contains_key(b"name"));
        assert!(!s.contains_key(b"age"));
    }

    #[test]
    fn with_one_too_big() {
        let big = vec![b'x'; SMALL_HASH_FIELD_MAX + 1];
        assert!(SmallHashData::with_one(&big, b"v").is_none());
    }

    #[test]
    fn set_add_update_remove() {
        let mut s = SmallHashData::new();
        assert!(matches!(s.try_set(b"a", b"1"), AddResult::Added));
        assert!(matches!(s.try_set(b"b", b"22"), AddResult::Added));
        assert!(matches!(s.try_set(b"a", b"X"), AddResult::Updated));
        assert_eq!(s.get(b"a"), Some(b"X".as_slice()));
        assert_eq!(s.get(b"b"), Some(b"22".as_slice()));
        assert!(s.try_remove(b"a"));
        assert!(!s.contains_key(b"a"));
        assert_eq!(s.get(b"b"), Some(b"22".as_slice()));
    }

    #[test]
    fn set_no_room_overflow() {
        let mut s = SmallHashData::new();
        // 8-byte field + 10-byte value = 2+8+10 = 20 bytes used.
        assert!(matches!(
            s.try_set(b"fieldnam", b"valuevalue"),
            AddResult::Added
        ));
        // Next pair won't fit.
        assert!(matches!(s.try_set(b"more", b"data"), AddResult::NoRoom));
    }

    #[test]
    fn update_value_grows_within_budget() {
        let mut s = SmallHashData::new();
        s.try_set(b"k", b"v");
        // grow from 1 byte value to 8 bytes — fits the budget.
        assert!(matches!(s.try_set(b"k", b"longerv!"), AddResult::Updated));
        assert_eq!(s.get(b"k"), Some(b"longerv!".as_slice()));
    }

    #[test]
    fn update_value_no_room() {
        let mut s = SmallHashData::new();
        // Fill near full.
        s.try_set(b"a", b"x");
        s.try_set(b"bbbbbbbb", b"yyyyyyyyyy"); // 8+10 = 18, +2 prefix = 20 + already 3 = 23 used? Let me reset.
        // Reset more deterministically:
        let mut s = SmallHashData::new();
        s.try_set(b"abc", b"defghijk"); // 2+3+8=13
        s.try_set(b"x", b"yzwuv"); // 2+1+5=8, total 21
        // Try to grow `x`'s value by 2 → would push used to 23 > 22.
        assert!(matches!(
            s.try_set(b"x", b"yzwuvAB"),
            AddResult::NoRoom
        ));
    }

    #[test]
    fn iter_in_insertion_order() {
        let mut s = SmallHashData::new();
        s.try_set(b"a", b"1");
        s.try_set(b"b", b"2");
        let v: Vec<(&[u8], &[u8])> = s.iter().collect();
        assert_eq!(v, vec![(b"a".as_slice(), b"1".as_slice()), (b"b".as_slice(), b"2".as_slice())]);
    }

    #[test]
    fn promote_preserves_pairs() {
        let mut s = SmallHashData::new();
        s.try_set(b"a", b"1");
        s.try_set(b"bb", b"22");
        let h = promote(&s);
        assert_eq!(h.len(), 2);
        assert_eq!(h.get(b"a".as_slice()).map(Vec::as_slice), Some(b"1".as_slice()));
        assert_eq!(h.get(b"bb".as_slice()).map(Vec::as_slice), Some(b"22".as_slice()));
    }
}
