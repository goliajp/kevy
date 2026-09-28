//! Transactions, propagation, the change feed and the wire-vs-embedded differential.

#[path = "../common/mod.rs"]
mod common;

mod aof_txn_markers;
mod differential_wire_vs_embedded;
mod feed_cdc;
mod spop_propagation;
mod watch;
