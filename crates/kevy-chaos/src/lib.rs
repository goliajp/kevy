//! Chaos test harness for kevy.
//!
//! See the crate-level [`README.md`](https://github.com/goliajp/kevy/blob/main/crates/kevy-chaos/README.md)
//! for context. Public surface is intentionally small — just enough to
//! spawn a kevy child, drive concurrent writes, simulate a crash, and
//! verify invariants on the recovered state.
//!
//! A crash run: write, kill the server mid-stream, restart it on the same
//! data dir, and check that every acknowledged write survived. It needs a
//! built kevy (`$KEVY_BIN`, or `target/release/kevy`), so it is compiled
//! here but not run.
//!
//! ```no_run
//! use kevy_chaos::{Harness, HarnessConfig, KillSignal, WriterPool, verify_all_present};
//! use std::sync::Arc;
//! use std::sync::atomic::{AtomicBool, Ordering};
//! use std::time::Duration;
//!
//! let dir = std::env::temp_dir().join("kevy-chaos-crash-run");
//! let mut kevy = Harness::spawn(HarnessConfig::new(dir.clone(), kevy_chaos::pick_free_port()))?;
//!
//! let stop = Arc::new(AtomicBool::new(false));
//! let pool = WriterPool::spawn(kevy.port(), 4, Arc::clone(&stop));
//! std::thread::sleep(Duration::from_secs(1));
//! kevy.kill(KillSignal::Sigkill)?;
//! stop.store(true, Ordering::Relaxed);
//! let log = pool.join();
//!
//! kevy.restart()?;
//! let acks = log.lock().expect("no writer panicked");
//! verify_all_present(kevy.port(), &acks)?;
//! # drop(kevy);
//! std::fs::remove_dir_all(&dir)?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod config;
mod harness;
mod proxy;
mod verify;
mod writer_pool;

pub use config::{HarnessConfig, KillSignal};
pub use harness::{Harness, pick_free_port};
pub use proxy::{ChaosProxy, Direction};
pub use verify::{pipelined_verify_counts, verify_all_present};
pub use writer_pool::{AckEntry, AckLog, WriterPool};
