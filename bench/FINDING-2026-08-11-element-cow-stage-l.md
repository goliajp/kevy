# FINDING 2026-08-11 — element-COW Stage L: the giant-list write stall is closed

Branch `feature/element-cow-list`, commit `987d7245`. RFC:
`.claude/rfcs/2026-08-11-post-v5-element-cow.md` (local). Upstream evidence:
`FINDING-2026-08-10-v5-rc-soak.md` ("the boundary": 0.7s → 7.4s → 9.5s reactor
gap stepping with single-collection size).

## The claim, measured

Phase A shape (box E2E, single connection, 64 B elements: fill N →
`BGREWRITEAOF` → 0.3 s into the view window, time one RPUSH round-trip):

| N | v5.0.0 whole-value COW | Stage L SegList | RSS transient |
|---|---:|---:|---|
| 1M | 8 ms (dump won the race) | **0.4 ms** | ~1.5 MB (was ~0) |
| 5M | **352 ms** | **0.4 ms** | ~1.8 MB (was **+341 MB**) |
| 20M | **666 ms** | **0.6 ms** | ~1.5 MB (was **+392 MB**) |

First write during a pinned view is now **size-independent** (~one segment:
16K elements ≈ 1 MB cloned) and the RSS transient is the segment, not a
second copy of the whole value. The second write in the same window is
0.2 ms (segment already unshared).

## What shipped

`Value::SegList(Arc<SegListData>)` — a deque of Arc-shared 16K-element
segments + O(1) length (`crates/kevy-store/src/list_seg.rs`). Lists promote
flat → segmented at 16K elements; ≤16K keeps the previous representation
byte-for-byte. Same RPUSH-stream / OP_LIST wire format (segmentation is a
memory fact, not a wire fact); `load_list` re-applies the switch on restore.
LTRIM releases whole out-of-range segments without cloning; LREM clones only
match-bearing segments. Unit proof: a write under a pinned view leaves
exactly one segment unshared (`tests_list_seg.rs`).

## Boundary that remains (per RFC stages)

Sets/hashes (Stage HS — bucket-sharded KevyMap), zsets (Stage Z — ranktree
path-copy), and streams (documented boundary) still pay whole-value COW.
The soak's `myset`/`myhash` cells reproduce until HS lands.
