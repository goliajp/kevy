//! Chaos test harness for kevy.
//!
//! See the crate-level [`README.md`](https://github.com/goliajp/kevy/blob/main/crates/kevy-chaos/README.md)
//! for context. Public surface is intentionally small — just enough to
//! spawn a kevy child, drive concurrent writes, simulate a crash, and
//! verify invariants on the recovered state.

mod harness;
mod writer_pool;

pub use harness::{Harness, HarnessConfig, KillSignal, pick_free_port};
pub use writer_pool::{AckEntry, AckLog, WriterPool, verify_all_present};
