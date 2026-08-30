//! `Value`'s two accounting questions: what does it weigh, and is freeing it
//! heavy enough to hand to the bio thread.
//!
//! Split from `value.rs` for the file-length rule. Both are pure per-variant
//! tables over the enum defined there, so they move as a pair and nothing in
//! `value.rs` calls them.

use crate::value::{
    HASH_SLOT_BYTES, HEAP_HEAVY_BYTES, LIST_SLOT_BYTES, RANKTREE_SLOT_BYTES, SET_SLOT_BYTES, Value,
    collection_overhead,
};

impl Value {
    /// Approximate heap bytes the value owns. Excludes the inline `Entry` /
    /// bucket slot — that's a separate per-entry constant accounted by the
    /// store. Walks collections, so prefer the cached `Entry::weight` for
    /// hot-path accounting and only call this when bootstrapping or after a
    /// load-from-snapshot.
    // LOC-WAIVER: pure per-variant weight table — one arm per stored encoding, no control flow
    pub fn weight(&self) -> u64 {
        match self {
            Value::Str(s) => s.heap_bytes() as u64,
            // i64 fits in the enum tag's space; no heap.
            Value::Int(_) => 0,
            // One payload buffer plus the boxed inner; nothing per column.
            Value::PackedRow(r) => r.heap_bytes() as u64,
            // Arc<[u8]> heap = the byte slice itself (refcount overhead
            // is amortised across shared clones).
            Value::ArcBulk(a) => a.len() as u64,
            Value::Hash(h) => {
                collection_overhead(h.capacity(), HASH_SLOT_BYTES)
                    + h.iter()
                        .map(|(f, v)| f.heap_bytes() as u64 + v.heap_bytes() as u64)
                        .sum::<u64>()
            }
            Value::List(l) => {
                (l.capacity() as u64).saturating_mul(LIST_SLOT_BYTES)
                    + l.iter().map(|v| v.capacity() as u64).sum::<u64>()
            }
            // Segments charge like flat lists; the outer deque-of-Arcs
            // adds one pointer slot per segment.
            Value::SegList(l) => {
                (l.seg_count() as u64).saturating_mul(8)
                    + (l.len() as u64).saturating_mul(LIST_SLOT_BYTES)
                    + l.iter().map(|v| v.capacity() as u64).sum::<u64>()
            }
            Value::Set(s) => {
                collection_overhead(s.capacity(), SET_SLOT_BYTES)
                    + s.iter().map(|m| m.heap_bytes() as u64).sum::<u64>()
            }
            Value::SegHash(h) => h.weight_as_hash(),
            Value::SegSet(s) => s.weight_as_set(),
            Value::SegZSet(z) => z.weight_as_zset(),
            // Inline collections live entirely in the Value variant
            // body — zero heap, zero bucket overhead. Accounting matches
            // `Value::Int` / inline `Value::Str` (both also return 0).
            Value::SmallSetInline(_)
            | Value::SmallHashInline(_)
            | Value::SmallListInline(_)
            | Value::SmallZSetInline(_) => 0,
            // The stub owns no heap — its 24 bytes live inline in the
            // Entry. The reclaimed value bytes are exactly the point:
            // a cold key weighs key-heap + ENTRY_OVERHEAD only (B7).
            Value::Cold(_) => 0,
            // Each member's bytes live twice when they spill to heap (>22 B):
            // once as the `by_member` key, once inside the rank tree's
            // `(Score, SmallBytes)` key — hence the ×2 on `heap_bytes`.
            // Members ≤22 B are inline in both slots (heap_bytes = 0).
            Value::ZSet(z) => {
                collection_overhead(z.by_member.capacity(), HASH_SLOT_BYTES)
                    + z.by_member.iter().map(|(m, _)| 2 * m.heap_bytes() as u64).sum::<u64>()
                    + (z.by_score.len() as u64).saturating_mul(RANKTREE_SLOT_BYTES)
            }
            Value::Stream(s) => s.weight(),
        }
    }

    /// Whether this value's `Drop` is heavy enough to deserve being
    /// shipped to the bio thread instead of freed inline. Fast: every
    /// variant decides off a sub-field cheap to inspect (no recursive
    /// walk), so it's safe to call on every overwrite-SET on the hot
    /// path. The threshold is intentionally conservative — small Arcs
    /// and every short string stay on inline-drop where jemalloc small-
    /// class is sub-µs and a cross-thread hand-off would lose.
    #[inline]
    // LOC-WAIVER: pure per-variant predicate table — one arm per stored encoding, no control flow
    pub fn is_heap_heavy(&self) -> bool {
        match self {
            // Inline 22 B / heap ≤ small-class — fast to free inline.
            Value::Str(_)
            | Value::Int(_)
            | Value::SmallSetInline(_)
            | Value::SmallHashInline(_)
            | Value::SmallListInline(_)
            | Value::SmallZSetInline(_) => false,
            // 24 inline bytes; dropping a stub frees nothing.
            Value::Cold(_) => false,
            // Lazy-drop's primary case: the large-value SET tail culprit.
            Value::ArcBulk(a) => a.len() >= HEAP_HEAVY_BYTES,
            // One buffer: the same size test, one deallocation.
            Value::PackedRow(r) => r.heap_bytes() >= HEAP_HEAVY_BYTES,
            // Collection drops walk every element + the bucket array;
            // worst-case microseconds on a multi-KB hash/zset. Send to
            // bio so a SET that overwrites a collection-typed key (the
            // Redis polymorphic case) doesn't stall the reactor.
            //
            // The check uses `Arc::strong_count == 1` to avoid sending
            // a still-shared Arc: another holder (a SnapshotView in
            // flight, a same-shard live read) would force the bio
            // thread to only do a refcount-decrement, which is wasted
            // cross-thread traffic. A unique Arc IS the case where
            // drop is expensive (it really frees the inner payload).
            Value::Hash(a) => alloc::sync::Arc::strong_count(a) == 1 && !a.is_empty(),
            Value::SegHash(a) => {
                alloc::sync::Arc::strong_count(a) == 1 && !a.is_empty() && a.all_unique()
            }
            Value::SegSet(a) => {
                alloc::sync::Arc::strong_count(a) == 1 && !a.is_empty() && a.all_unique()
            }
            Value::SegZSet(a) => {
                alloc::sync::Arc::strong_count(a) == 1 && !a.is_empty() && a.all_unique()
            }
            Value::List(a) => alloc::sync::Arc::strong_count(a) == 1 && !a.is_empty(),
            // Bio-drop only pays off when the drop really frees: outer
            // AND every segment unique. A view-shared SegList's drop is
            // refcount decrements — cheap enough inline.
            Value::SegList(a) => {
                alloc::sync::Arc::strong_count(a) == 1 && !a.is_empty() && a.all_unique()
            }
            Value::Set(a) => alloc::sync::Arc::strong_count(a) == 1 && !a.is_empty(),
            Value::ZSet(a) => alloc::sync::Arc::strong_count(a) == 1 && !a.by_member.is_empty(),
            Value::Stream(a) => alloc::sync::Arc::strong_count(a) == 1 && a.length() > 0,
        }
    }
}
