# kevy-ranktree

An **order-statistic B-tree**: an ordered set of `K: Ord` where rank queries
are as cheap as lookups.

`std::collections::BTreeSet` orders your keys but cannot answer "what
position is this key at?" without an O(N) walk — it is not augmentable.
`kevy-ranktree` is a hand-written B-tree whose every node carries its
subtree element count, so the same O(log N) descent that finds a key also
counts everything to its left.

| operation | cost |
|---|---|
| `insert` / `remove` | O(log N) |
| `rank_of(&key)` — position of a key | O(log N) |
| `select(rank)` — k-th smallest key | O(log N) |
| `partition_point(pred)` / `count_in(bounds)` | O(log N) |
| `range(bounds)` / `iter_from(rank)` | O(log N) seek, then O(1) amortised per item |
| `iter()` / `iter_rev()` | O(1) amortised per item |

Born as the backing structure for kevy's sorted sets (`ZRANK`, `ZRANGE`,
`ZCOUNT`, `ZRANGEBYSCORE` — the leaderboard workload), keyed there by
`(score, member)`; the tree itself is generic over any `K: Ord`.

## Design

A counted B-tree rather than a spanned skiplist (Redis's choice): nodes hold
up to 15 keys in contiguous arrays, so descents touch ~log₈(N) cache lines
and ordered scans iterate arrays instead of chasing per-element pointers,
and the balance bounds are deterministic rather than probabilistic. The
price — B-tree delete rebalancing — is covered by a randomized oracle test
(`tests/oracle.rs`) replaying thousands of operations against a sorted-`Vec`
reference, plus white-box invariant checks after every mutation shape.

## Constraints

Pure Rust, zero dependencies, `#![forbid(unsafe_code)]`, `no_std`-capable
(`--no-default-features --features alloc`).

## License

Apache-2.0 OR MIT, at your option.
