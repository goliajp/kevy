//! `SmallZSetData` — inline-listpack encoding for tiny sorted sets.
//!
//! Companion to [`crate::small_set::SmallSetData`]. Mirrors valkey's
//! `OBJ_ENCODING_LISTPACK` for zsets (`t_zset.c::zsetTypeMaybeConvert`).
//! Each tuple is `[score: f64 (8 B)][len: u8][member]` — 9 bytes of
//! overhead before any member bytes.
//!
//! The 22-byte budget thus fits at most 1 (9 + len ≤ 22 → len ≤ 13)
//! member if it's short, or up to 2 members for very short ones.
//! For the `redis-benchmark -t zadd` default literal-member shape
//! (`element:__rand_int__`, 20 bytes), only the **single-element**
//! ZADD pattern fits inline (9 + 1 + 12 = 22 — exact fit). The first
//! ZADD on a fresh key wins, second triggers promotion to KevyMap +
//! BTreeSet.
//!
//! Members are NOT sorted in the inline buffer; we only have at most
//! a few entries, and `iter()`/`zrange()` simply scans them. When
//! promotion happens, the BTreeSet path takes over and applies the
//! correct ordering.
//!
//! ## Layout — 24 bytes packed
//!
//! ```text
//! offset: 0    1                                 23
//!         +----+----+----+----+ ... +----+ ... +-----+
//!         | n  | u  |  buf[22]                       |
//!         +----+----+----+----+----+ ...     +-----+
//! ```
//!
//! - `n` (u8): tuple count.
//! - `u` (u8): bytes used (sum of `9 + len_i`).
//! - `buf` ([u8; 22]): packed `[score:8][len:1][member:len]` entries.

/// Inline packed sorted-set storage. 24 bytes total.
#[derive(Clone)]
pub struct SmallZSetData {
    count: u8,
    used: u8,
    buf: [u8; SMALL_ZSET_BUF_CAP],
}

pub(crate) const SMALL_ZSET_BUF_CAP: usize = 22;
/// Per-member cap: buf_cap minus the 9-byte (score + length prefix)
/// fixed overhead.
pub(crate) const SMALL_ZSET_MEMBER_MAX: usize = SMALL_ZSET_BUF_CAP - 9;
pub(crate) const SMALL_ZSET_COUNT_MAX: usize = 2;

/// Outcome of [`SmallZSetData::try_set`].
pub(crate) enum AddResult {
    /// Member was new — count + used updated.
    Added,
    /// Member existed — score was updated in place.
    Updated,
    /// Doesn't fit (oversized member, count cap, or budget overflow).
    NoRoom,
}

impl SmallZSetData {
    pub(crate) fn new() -> Self {
        Self { count: 0, used: 0, buf: [0; SMALL_ZSET_BUF_CAP] }
    }

    pub(crate) fn with_one(member: &[u8], score: f64) -> Option<Self> {
        if member.len() > SMALL_ZSET_MEMBER_MAX {
            return None;
        }
        let mut s = Self::new();
        s.write_pair_at(0, member, score);
        s.count = 1;
        s.used = (9 + member.len()) as u8;
        Some(s)
    }

    pub fn len(&self) -> usize {
        self.count as usize
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Look up `member`'s score.
    pub fn score(&self, member: &[u8]) -> Option<f64> {
        for (m, s) in self.iter() {
            if m == member {
                return Some(s);
            }
        }
        None
    }

    /// Does the inline zset contain `member`?
    pub fn contains(&self, member: &[u8]) -> bool {
        self.score(member).is_some()
    }

    /// Iterator yielding `(&[u8] member, f64 score)` in insertion order.
    pub fn iter(&self) -> SmallZSetIter<'_> {
        SmallZSetIter { buf: &self.buf[..self.used as usize], cursor: 0 }
    }

    /// Try to set `member -> score`. See [`AddResult`].
    pub(crate) fn try_set(&mut self, member: &[u8], score: f64) -> AddResult {
        if member.len() > SMALL_ZSET_MEMBER_MAX {
            return AddResult::NoRoom;
        }
        if let Some(off) = self.locate(member) {
            // In-place score update: rewrite the 8-byte score prefix.
            self.buf[off..off + 8].copy_from_slice(&score.to_bits().to_le_bytes());
            return AddResult::Updated;
        }
        if self.count as usize >= SMALL_ZSET_COUNT_MAX {
            return AddResult::NoRoom;
        }
        let need = 9 + member.len();
        let new_used = self.used as usize + need;
        if new_used > SMALL_ZSET_BUF_CAP {
            return AddResult::NoRoom;
        }
        let off = self.used as usize;
        self.write_pair_at(off, member, score);
        self.used = new_used as u8;
        self.count += 1;
        AddResult::Added
    }

    /// Try to remove `member`. Returns whether it was present.
    pub(crate) fn try_remove(&mut self, member: &[u8]) -> bool {
        let Some(off) = self.locate(member) else {
            return false;
        };
        let used = self.used as usize;
        let len = self.buf[off + 8] as usize;
        let entry_end = off + 9 + len;
        self.buf.copy_within(entry_end..used, off);
        let shifted = used - entry_end;
        let new_used = off + shifted;
        self.buf[new_used..used].fill(0);
        self.used = new_used as u8;
        self.count -= 1;
        true
    }

    fn write_pair_at(&mut self, off: usize, member: &[u8], score: f64) {
        self.buf[off..off + 8].copy_from_slice(&score.to_bits().to_le_bytes());
        self.buf[off + 8] = member.len() as u8;
        let mstart = off + 9;
        let mend = mstart + member.len();
        self.buf[mstart..mend].copy_from_slice(member);
    }

    /// Returns entry offset (start of score) if present.
    fn locate(&self, member: &[u8]) -> Option<usize> {
        let mut cursor = 0usize;
        let used = self.used as usize;
        while cursor < used {
            let len = self.buf[cursor + 8] as usize;
            let mstart = cursor + 9;
            let mend = mstart + len;
            if &self.buf[mstart..mend] == member {
                return Some(cursor);
            }
            cursor = mend;
        }
        None
    }
}

/// Iterator over [`SmallZSetData`] yielding `(member, score)`.
pub struct SmallZSetIter<'a> {
    buf: &'a [u8],
    cursor: usize,
}

impl<'a> Iterator for SmallZSetIter<'a> {
    type Item = (&'a [u8], f64);
    fn next(&mut self) -> Option<Self::Item> {
        if self.cursor >= self.buf.len() {
            return None;
        }
        let mut score_bytes = [0u8; 8];
        score_bytes.copy_from_slice(&self.buf[self.cursor..self.cursor + 8]);
        let score = f64::from_bits(u64::from_le_bytes(score_bytes));
        let len = self.buf[self.cursor + 8] as usize;
        let mstart = self.cursor + 9;
        let mend = mstart + len;
        self.cursor = mend;
        Some((&self.buf[mstart..mend], score))
    }
}

/// Materialise the inline zset as a heap-backed [`crate::value::ZSetData`].
pub(crate) fn promote(inline: &SmallZSetData) -> crate::value::ZSetData {
    let mut z = crate::value::ZSetData::default();
    for (m, sc) in inline.iter() {
        z.insert(m, sc);
    }
    z
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_is_24_bytes() {
        assert_eq!(std::mem::size_of::<SmallZSetData>(), 24);
    }

    #[test]
    fn add_and_score() {
        let mut z = SmallZSetData::new();
        assert!(matches!(z.try_set(b"a", 1.0), AddResult::Added));
        assert!(matches!(z.try_set(b"b", 2.5), AddResult::Added));
        assert_eq!(z.score(b"a"), Some(1.0));
        assert_eq!(z.score(b"b"), Some(2.5));
        assert!(z.contains(b"a"));
        assert!(!z.contains(b"c"));
    }

    #[test]
    fn update_in_place() {
        let mut z = SmallZSetData::new();
        z.try_set(b"a", 1.0);
        assert!(matches!(z.try_set(b"a", 9.5), AddResult::Updated));
        assert_eq!(z.score(b"a"), Some(9.5));
        assert_eq!(z.len(), 1);
    }

    #[test]
    fn count_cap() {
        let mut z = SmallZSetData::new();
        z.try_set(b"a", 1.0);
        z.try_set(b"b", 2.0);
        // Count cap = 2, third add should NoRoom.
        assert!(matches!(z.try_set(b"c", 3.0), AddResult::NoRoom));
    }

    #[test]
    fn member_too_long() {
        let mut z = SmallZSetData::new();
        let big = vec![b'x'; SMALL_ZSET_MEMBER_MAX + 1];
        assert!(matches!(z.try_set(&big, 1.0), AddResult::NoRoom));
    }

    #[test]
    fn budget_overflow() {
        let mut z = SmallZSetData::new();
        // 1 member at 13 bytes uses 9+13=22 of 22.
        let m1 = vec![b'a'; SMALL_ZSET_MEMBER_MAX];
        assert!(matches!(z.try_set(&m1, 1.0), AddResult::Added));
        // Second member: even 1-byte member needs 10 bytes → NoRoom.
        assert!(matches!(z.try_set(b"b", 2.0), AddResult::NoRoom));
    }

    #[test]
    fn remove_works() {
        let mut z = SmallZSetData::new();
        z.try_set(b"a", 1.0);
        z.try_set(b"b", 2.0);
        assert!(z.try_remove(b"a"));
        assert!(!z.contains(b"a"));
        assert_eq!(z.score(b"b"), Some(2.0));
        assert_eq!(z.len(), 1);
    }

    #[test]
    fn iter_order() {
        let mut z = SmallZSetData::new();
        z.try_set(b"b", 2.0);
        z.try_set(b"a", 1.0);
        let v: Vec<(&[u8], f64)> = z.iter().collect();
        assert_eq!(v[0], (b"b".as_slice(), 2.0));
        assert_eq!(v[1], (b"a".as_slice(), 1.0));
    }

    #[test]
    fn promote_preserves_pairs() {
        let mut z = SmallZSetData::new();
        z.try_set(b"a", 1.0);
        z.try_set(b"b", 2.0);
        let zd = promote(&z);
        assert_eq!(zd.len(), 2);
    }
}
