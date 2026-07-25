//! Test-suite module declarations, split out of `store.rs` for the
//! 500-LOC house rule. `#[path]` is relative to this file, which sits
//! beside every suite it names, so the paths are unchanged.
//!
//! Suites reach shared helpers as `super::tests::tmp_dir` -- `super` is
//! this module, and `tests` is a sibling declared right here.

#[path = "store_tests.rs"]
mod tests;
#[path = "store_tests_shard.rs"]
mod tests_shard;
#[path = "store_tests_p2.rs"]
mod tests_p2;
#[path = "store_tests_p3.rs"]
mod tests_p3;
#[path = "store_tests_bitmap.rs"]
mod tests_bitmap;
#[path = "store_tests_bonus.rs"]
mod tests_bonus;
#[path = "store_tests_scan.rs"]
mod tests_scan;
#[path = "store_tests_atomic.rs"]
mod tests_atomic;
#[path = "store_tests_more.rs"]
mod tests_more;
#[path = "store_tests_keyspace.rs"]
mod tests_keyspace;
#[path = "store_tests_atomic_all.rs"]
mod tests_atomic_all;
#[cfg(all(test, feature = "index"))]
#[path = "store_tests_atomic_index.rs"]
mod tests_atomic_index;
#[path = "store_tests_reconcile.rs"]
mod tests_reconcile;
#[path = "store_tests_replay_all.rs"]
mod tests_replay_all;
#[path = "store_tests_op_table.rs"]
mod tests_op_table;
