//! A5: 0-dep small-vector for ArgvBorrowed's `(start, end)` range table.
//!
//! Most commands have ≤4 args (PING, GET k, SET k v, MGET k1 k2 k3, …). The
//! prior `Vec<(u32, u32)>` heap-allocated once per parsed command — at ~76 k
//! req/s on the lx64 -c1 hot path that's a steady malloc/free per request that
//! H1 (`perf c2c`) confirmed shows up as cross-thread libc cfree contention.
//! Storing the first 4 ranges inline (one cache line: 4 × 8 = 32 bytes plus a
//! `usize` len) drops the per-command alloc to zero for the common shape and
//! spills to a `Vec` only when argc > 4.

/// Inline 4-slot range table for [`crate::ArgvBorrowed`].
///
/// Layout:
///   - `inline[0..min(len, INLINE_CAP)]` is the live range — `(u32, u32)` is
///     `Copy + !Drop`, so zero-init for the trailing slots is free and stays
///     within `#![forbid(unsafe_code)]`
///   - if `len <= INLINE_CAP`, `heap` is empty
///   - if `len > INLINE_CAP`, the first INLINE_CAP entries live in `inline`
///     and the remaining `len - INLINE_CAP` live in `heap`
///
/// This shape keeps the hot push path branchless for the common ≤4 case and
/// lets `get(i)` index either side with a single compare.
pub(crate) struct InlineRanges {
    inline: [(u32, u32); INLINE_CAP],
    heap: Vec<(u32, u32)>,
    len: usize,
}

const INLINE_CAP: usize = 4;

impl InlineRanges {
    /// Empty table (no allocations).
    pub(crate) fn new() -> Self {
        Self {
            inline: [(0, 0); INLINE_CAP],
            heap: Vec::new(),
            len: 0,
        }
    }

    /// Empty table, pre-reserving `cap` heap slots if `cap > INLINE_CAP`. The
    /// inline tier is always available regardless of the hint.
    pub(crate) fn with_capacity(cap: usize) -> Self {
        let heap = if cap > INLINE_CAP {
            Vec::with_capacity(cap - INLINE_CAP)
        } else {
            Vec::new()
        };
        Self {
            inline: [(0, 0); INLINE_CAP],
            heap,
            len: 0,
        }
    }

    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Append one range.
    #[inline]
    pub(crate) fn push(&mut self, value: (u32, u32)) {
        if self.len < INLINE_CAP {
            self.inline[self.len] = value;
        } else {
            self.heap.push(value);
        }
        self.len += 1;
    }

    /// Random access. Returns `None` for out-of-bounds.
    #[inline]
    pub(crate) fn get(&self, i: usize) -> Option<(u32, u32)> {
        if i >= self.len {
            return None;
        }
        if i < INLINE_CAP {
            Some(self.inline[i])
        } else {
            Some(self.heap[i - INLINE_CAP])
        }
    }
}

impl Clone for InlineRanges {
    fn clone(&self) -> Self {
        let mut out = Self::with_capacity(self.len);
        for i in 0..self.len {
            out.push(self.get(i).expect("in range"));
        }
        out
    }
}

impl core::fmt::Debug for InlineRanges {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut dl = f.debug_list();
        for i in 0..self.len {
            dl.entry(&self.get(i).expect("in range"));
        }
        dl.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_has_zero_len() {
        let r = InlineRanges::new();
        assert_eq!(r.len(), 0);
        assert!(r.is_empty());
        assert!(r.get(0).is_none());
    }

    #[test]
    fn push_below_inline_cap_uses_no_heap() {
        let mut r = InlineRanges::new();
        for i in 0..INLINE_CAP {
            r.push((i as u32, i as u32 + 1));
        }
        assert_eq!(r.len(), INLINE_CAP);
        assert!(!r.is_empty());
        assert_eq!(r.heap.len(), 0);
        for i in 0..INLINE_CAP {
            assert_eq!(r.get(i), Some((i as u32, i as u32 + 1)));
        }
    }

    #[test]
    fn push_spills_to_heap_when_full() {
        let mut r = InlineRanges::new();
        for i in 0..10 {
            r.push((i as u32, i as u32 + 1));
        }
        assert_eq!(r.len(), 10);
        assert_eq!(r.heap.len(), 10 - INLINE_CAP);
        for i in 0..10 {
            assert_eq!(r.get(i), Some((i as u32, i as u32 + 1)));
        }
        assert!(r.get(10).is_none());
    }

    #[test]
    fn with_capacity_preallocates_heap_only_beyond_inline() {
        let r = InlineRanges::with_capacity(2);
        assert_eq!(r.heap.capacity(), 0);
        let r = InlineRanges::with_capacity(INLINE_CAP);
        assert_eq!(r.heap.capacity(), 0);
        let r = InlineRanges::with_capacity(INLINE_CAP + 5);
        assert!(r.heap.capacity() >= 5);
    }

    #[test]
    fn clone_preserves_entries_across_tiers() {
        let mut r = InlineRanges::new();
        for i in 0..7 {
            r.push((i as u32, i as u32 + 1));
        }
        let c = r.clone();
        assert_eq!(c.len(), 7);
        for i in 0..7 {
            assert_eq!(c.get(i), Some((i as u32, i as u32 + 1)));
        }
    }
}
