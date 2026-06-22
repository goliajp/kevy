//! `SmallSetData` — valkey-style inline-listpack encoding for tiny sets.
//!
//! v1.25 A.7 O5: mirror valkey's `OBJ_ENCODING_LISTPACK` for sets of size
//! 1–N (`t_set.c::setTypeMaybeConvert`). valkey starts a fresh set as a
//! 1-entry listpack inside one cache line; once cardinality grows past
//! `set-max-listpack-entries` (128 default) OR a single member exceeds the
//! per-entry size cap, it converts to `OBJ_ENCODING_HT`. kevy's analogue:
//! [`SmallSetData`] for tiny sets, upgrade to [`crate::value::SetData`]
//! (Swiss-table `KevySet<SmallBytes>`) on overflow.
//!
//! ## Layout
//!
//! Exactly 24 bytes, mirroring [`kevy_bytes::SmallBytes`] so the
//! `Value::SmallSetInline(SmallSetData)` variant body matches the size
//! of `Value::Str(SmallBytes)` and the
//! `assert!(size_of::<Value>() <= 32)` in `value.rs:162` still holds:
//!
//! ```text
//! offset: 0    1                                 23
//!         +----+----+----+----+----+ ...     +-----+
//!         | n  | u  |       buf[22]              |
//!         +----+----+----+----+----+ ...     +-----+
//! ```
//!
//! - `n` (u8): member count (`0..=22`, capped well below to leave room).
//! - `u` (u8): bytes used in `buf` (sum of `1 + len_i` over all members).
//! - `buf` ([u8; 22]): packed `[len_i: u8][member_i: u8; len_i]` entries.
//!
//! Per-entry length prefix is one byte → a single member is at most 21
//! bytes (need 1 byte for the length itself). For 20-byte
//! `element:__rand_int__` (the redis-benchmark default SADD member shape),
//! one entry consumes 21 bytes — fits with 1 spare. For shorter members,
//! 4-5 fit comfortably.
//!
//! ## Upgrade trigger
//!
//! Insert returns [`AddResult::NoRoom`] when either (a) the new member's
//! `1 + len` would overflow the 22-byte budget, or (b) the per-member
//! length exceeds the 21-byte per-entry cap. The caller upgrades to
//! `Value::Set(Arc<SetData>)` and re-inserts the new member there.
//!
//! Linear-scan `contains` over ≤ N members is faster than the
//! hash-then-SIMD-probe path on a 16-slot Swiss table when N is small
//! AND the data is one cache line — the same structural reason valkey's
//! listpack beats `OBJ_ENCODING_HT` for N≤128.
//!
//! ## Future extension
//!
//! Hash / List / ZSet families want the same encoding switch. The
//! pattern factored here:
//! 1. Inline 24-byte packed-entries variant on `Value`.
//! 2. Per-op `try_*` helper returns `AddResult { Added, AlreadyPresent,
//!    NoRoom }` so the caller knows when to upgrade.
//! 3. `account_delta` accepts the per-member weight delta uniformly; the
//!    inline variant returns its own `inline_weight()` (zero heap) so the
//!    Store's `used_memory` accounting stays consistent across both
//!    encodings.
//!
//! The `Hash` analogue will need `[len_field][field][len_val][val]`
//! tuples; List can use the same `[len][bytes]` shape; ZSet needs
//! `[score:8][len][member]`. All three fit the 24-byte budget for the
//! 1–3 member cases that dominate `redis-benchmark` default shapes.

use kevy_bytes::SmallBytes;

/// Inline packed set storage. 24 bytes total — see module docs for layout.
#[derive(Clone)]
pub struct SmallSetData {
    /// Number of inline members (0..=22 cap, real ceiling is byte-budget).
    count: u8,
    /// Bytes used in `buf` so far (sum of `1 + member_len` per entry).
    used: u8,
    /// Packed `[len_i: u8][member_i; len_i]` entries, contiguous from
    /// offset 0 up to `used`.
    buf: [u8; SMALL_SET_BUF_CAP],
}

/// Byte budget for the inline packed entries area. Chosen so that
/// `count(1) + used(1) + buf(22) = 24` bytes total, matching the
/// `SmallBytes` body size and preserving `size_of::<Value>() <= 32`.
pub(crate) const SMALL_SET_BUF_CAP: usize = 22;

/// Per-member length cap: one byte is spent on the length prefix, so the
/// member payload can be at most `SMALL_SET_BUF_CAP - 1` bytes. Members
/// larger than this trigger an [`AddResult::NoRoom`] regardless of how
/// empty the inline buffer is — the caller must upgrade to
/// `Value::Set` to store them.
pub(crate) const SMALL_SET_MEMBER_MAX: usize = SMALL_SET_BUF_CAP - 1;

/// Per-set member count cap. The byte budget tends to hit first (a single
/// 20-byte member takes 21 of 22 bytes), but a hard `count` cap keeps the
/// linear scan deterministic and bounds the `u8` count field.
pub(crate) const SMALL_SET_COUNT_MAX: usize = 8;

/// Outcome of [`SmallSetData::try_add`].
pub(crate) enum AddResult {
    /// Member was new; count + used updated.
    Added,
    /// Member already present; no change.
    AlreadyPresent,
    /// Member doesn't fit (either too long or buffer full / count cap).
    /// Caller must upgrade to `Value::Set` and re-insert.
    NoRoom,
}

impl SmallSetData {
    /// Build an empty inline set.
    pub(crate) fn new() -> Self {
        Self {
            count: 0,
            used: 0,
            buf: [0; SMALL_SET_BUF_CAP],
        }
    }

    /// Build an inline set holding one member, if it fits. Returns `None`
    /// when the member exceeds [`SMALL_SET_MEMBER_MAX`] — the caller
    /// should create a `Value::Set(Arc::default())` and insert the
    /// member there instead.
    pub(crate) fn with_one(member: &[u8]) -> Option<Self> {
        if member.len() > SMALL_SET_MEMBER_MAX {
            return None;
        }
        let mut s = Self::new();
        s.buf[0] = member.len() as u8;
        s.buf[1..1 + member.len()].copy_from_slice(member);
        s.count = 1;
        s.used = 1 + member.len() as u8;
        Some(s)
    }

    /// Number of inline members.
    pub fn len(&self) -> usize {
        self.count as usize
    }

    /// Whether the inline set has no members.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Linear scan for `member`. ≤22 bytes of packed entries fit in one
    /// cache line; loop is unrolled by the optimiser at small counts.
    pub fn contains(&self, member: &[u8]) -> bool {
        self.iter_slices().any(|m| m == member)
    }

    /// Iterator over the packed entries as `&[u8]` slices. Owns nothing.
    pub fn iter_slices(&self) -> SmallSetIter<'_> {
        SmallSetIter { buf: &self.buf[..self.used as usize], cursor: 0 }
    }

    /// Alias for [`Self::iter_slices`] — matches the `iter` shape used by
    /// other collection types in this crate.
    pub fn iter(&self) -> SmallSetIter<'_> {
        self.iter_slices()
    }

    /// Try to append `member`. See [`AddResult`].
    pub(crate) fn try_add(&mut self, member: &[u8]) -> AddResult {
        if self.contains(member) {
            return AddResult::AlreadyPresent;
        }
        if member.len() > SMALL_SET_MEMBER_MAX {
            return AddResult::NoRoom;
        }
        if self.count as usize >= SMALL_SET_COUNT_MAX {
            return AddResult::NoRoom;
        }
        let need = 1 + member.len();
        let new_used = self.used as usize + need;
        if new_used > SMALL_SET_BUF_CAP {
            return AddResult::NoRoom;
        }
        let off = self.used as usize;
        self.buf[off] = member.len() as u8;
        self.buf[off + 1..off + need].copy_from_slice(member);
        self.used = new_used as u8;
        self.count += 1;
        AddResult::Added
    }

    /// Try to remove `member`. Returns whether it was present. On hit,
    /// the trailing packed entries are shifted left by `1 + len` to
    /// close the gap (deterministic O(used) memmove, fits in cache line).
    pub(crate) fn try_remove(&mut self, member: &[u8]) -> bool {
        let mut cursor = 0usize;
        let used = self.used as usize;
        while cursor < used {
            let len = self.buf[cursor] as usize;
            let start = cursor + 1;
            let end = start + len;
            if &self.buf[start..end] == member {
                // Shift [end..used) → [cursor..)
                self.buf.copy_within(end..used, cursor);
                let shifted = used - end;
                let new_used = cursor + shifted;
                // Zero the freed tail (avoid leaking old member bytes
                // into snapshots / accidental Debug prints).
                self.buf[new_used..used].fill(0);
                self.used = new_used as u8;
                self.count -= 1;
                return true;
            }
            cursor = end;
        }
        false
    }
}

/// Iterator over [`SmallSetData`] members as `&[u8]` slices.
pub struct SmallSetIter<'a> {
    buf: &'a [u8],
    cursor: usize,
}

impl<'a> Iterator for SmallSetIter<'a> {
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

/// Materialise the inline set as a heap-backed [`crate::value::SetData`].
/// Used when an upgrade is forced by an oversized member or full buffer.
pub(crate) fn promote(inline: &SmallSetData) -> crate::value::SetData {
    let mut s = crate::value::SetData::with_capacity(inline.len().max(1));
    for m in inline.iter_slices() {
        s.insert(SmallBytes::from_slice(m));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_is_24_bytes() {
        // Mirrors SmallBytes' 24 B body so size_of::<Value>() <= 32 holds.
        assert_eq!(std::mem::size_of::<SmallSetData>(), 24);
    }

    #[test]
    fn empty_and_with_one() {
        let s = SmallSetData::new();
        assert_eq!(s.len(), 0);
        assert!(!s.contains(b"foo"));

        let s = SmallSetData::with_one(b"hi").unwrap();
        assert_eq!(s.len(), 1);
        assert!(s.contains(b"hi"));
        assert!(!s.contains(b"hj"));
    }

    #[test]
    fn member_too_long_for_with_one() {
        let big = vec![b'x'; SMALL_SET_MEMBER_MAX + 1];
        assert!(SmallSetData::with_one(&big).is_none());
    }

    #[test]
    fn add_dedup_and_iter() {
        let mut s = SmallSetData::new();
        assert!(matches!(s.try_add(b"a"), AddResult::Added));
        assert!(matches!(s.try_add(b"b"), AddResult::Added));
        assert!(matches!(s.try_add(b"a"), AddResult::AlreadyPresent));
        assert_eq!(s.len(), 2);
        let v: Vec<&[u8]> = s.iter_slices().collect();
        assert_eq!(v, vec![b"a".as_slice(), b"b".as_slice()]);
    }

    #[test]
    fn full_buffer_returns_no_room() {
        let mut s = SmallSetData::new();
        // single 20-byte member uses 21 of 22 bytes; second won't fit.
        let m1 = b"element:__rand_int__";
        assert_eq!(m1.len(), 20);
        assert!(matches!(s.try_add(m1), AddResult::Added));
        assert!(matches!(s.try_add(b"x"), AddResult::NoRoom));
    }

    #[test]
    fn member_too_long_returns_no_room() {
        let mut s = SmallSetData::new();
        let big = vec![b'x'; SMALL_SET_MEMBER_MAX + 1];
        assert!(matches!(s.try_add(&big), AddResult::NoRoom));
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn remove_middle_shifts_tail() {
        let mut s = SmallSetData::new();
        assert!(matches!(s.try_add(b"aa"), AddResult::Added));
        assert!(matches!(s.try_add(b"bbb"), AddResult::Added));
        assert!(matches!(s.try_add(b"cc"), AddResult::Added));
        assert!(s.try_remove(b"bbb"));
        assert_eq!(s.len(), 2);
        assert!(s.contains(b"aa"));
        assert!(!s.contains(b"bbb"));
        assert!(s.contains(b"cc"));
        let v: Vec<&[u8]> = s.iter_slices().collect();
        assert_eq!(v, vec![b"aa".as_slice(), b"cc".as_slice()]);
    }

    #[test]
    fn remove_absent_returns_false() {
        let mut s = SmallSetData::new();
        s.try_add(b"a");
        assert!(!s.try_remove(b"zz"));
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn count_cap_returns_no_room() {
        let mut s = SmallSetData::new();
        // 8 × 1-byte members fit in 16 bytes; 9th should hit the count cap
        // before the byte cap.
        for c in b"abcdefgh" {
            assert!(matches!(s.try_add(&[*c]), AddResult::Added));
        }
        assert_eq!(s.len(), SMALL_SET_COUNT_MAX);
        assert!(matches!(s.try_add(b"i"), AddResult::NoRoom));
    }

    #[test]
    fn promote_preserves_members() {
        let mut s = SmallSetData::new();
        s.try_add(b"a");
        s.try_add(b"bb");
        s.try_add(b"ccc");
        let promoted = promote(&s);
        assert_eq!(promoted.len(), 3);
        assert!(promoted.contains(b"a".as_slice()));
        assert!(promoted.contains(b"bb".as_slice()));
        assert!(promoted.contains(b"ccc".as_slice()));
    }
}
