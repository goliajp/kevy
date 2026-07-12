# Changelog

All notable changes to **kevy-ranktree** will be documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Initial release: `RankTree<K: Ord>` — an order-statistic B-tree
  (per-node subtree counts) with O(log N) `insert` / `remove` /
  `rank_of` / `select` / `partition_point` / `count_in`, and forward /
  reverse in-order iterators seekable to any rank in O(log N)
  (`iter_from`, `iter_rev_from`, `range`).
- `no_std` support behind the `alloc` feature; `#![forbid(unsafe_code)]`.
- Randomized oracle test suite against a sorted-`Vec` reference and
  white-box structural invariant checks.
