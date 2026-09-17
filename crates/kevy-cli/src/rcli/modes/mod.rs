//! The special modes: jobs that run against the server instead of a command
//! or the REPL (`--scan`, `--bigkeys`, `--stat`, …).

mod bigkeys;
pub(crate) mod dispatch;
pub(crate) mod eval;
mod hdr;
mod hotkeys;
mod human;
mod intrinsic;
mod keystats;
mod keystats_report;
mod latency;
mod latency_dist;
mod lru_test;
mod pages;
mod pipe;
mod progress;
mod random;
mod rdb;
mod reconnect;
mod replica;
mod scan;
mod server;
mod sizes;
mod stat;
mod sync;
mod vset_recall;
mod wire;
