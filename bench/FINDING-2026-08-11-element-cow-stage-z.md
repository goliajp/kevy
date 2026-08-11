# FINDING 2026-08-11 — element-COW Stage Z: the giant-zset write stall is closed

Branch `feature/element-cow-z`. Stage L/HS twins:
`FINDING-2026-08-11-element-cow-stage-l.md`, `-stage-hs.md`. RFC: element-COW
under `.claude/rfcs/` (local).

## The claim, measured

Same probe shape (box E2E, fill N ZADD members → `BGREWRITEAOF` → 0.3 s into
the view window, time one ZADD round-trip):

| N | first write during view | second write | RSS transient |
|---:|---:|---:|---|
| 1M | 0.7 ms | 0.42 ms | ~3.5 MB |
| 5M | 0.8 ms | 0.29 ms | ~1.5 MB |
| 20M | **1.0 ms** | 0.26 ms | ~1.5 MB |

Size-independent. Before this stage the same write deep-cloned the whole
`ZSetData` — the member `KevyMap` layout copy PLUS the entire recursive
rank B-tree — on the serving thread.

## What shipped

`Value::SegZSet` (`crates/kevy-store/src/zset_seg.rs`) — a composition of
the two existing stones, neither forked: member→score is a Stage-HS
`SegMap<f64>`; the score order is a vector of `Arc`-shared `RankTree`
segments over contiguous `(score, member)` ranges (≤16K entries each,
cached max keys for O(log segments) routing). A write clones one member
bucket + one segment tree. Rank arithmetic pays an O(segments) prefix walk
(µs at 100M members); score-bound seeks answer whole segments from cached
maxes and descend only the frontier segment. Promotion at 16K members from
ZADD, the ZINCRBY-only path, and restore; ≤16K keeps the flat
representation byte-for-byte; wire formats unchanged.

## Gates

kevy-store 167 + kevy-persist 69 tests green (COW one-segment claim proven
in `tests_zset_seg.rs`); locgate/clippy/commentgate clean. perfgate +
branch CI recorded below before merge.

## Boundary that remains

Streams keep whole-value COW (documented boundary per the RFC — the
XADD/XTRIM working set is bounded in practice; a `SegStream` would need a
COW B-tree fork and earns its own arc if evidence ever demands it).
