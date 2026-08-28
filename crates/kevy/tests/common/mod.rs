//! Waiting on a shard, without writing down a number of milliseconds.
//!
//! Two things in this engine become visible to a test only after a shard
//! TICKS, and several cells waited for them with a bare sleep.
//!
//! `INFO` is answered on one shard. That shard refreshes its own slot and
//! then SUMS every shard's slot (`ops::info` -> `stats::publish_gauges` ->
//! `obs.aggregate`), so the other seven are only as fresh as their last
//! tick. Live config is the same shape: `config_replace` is picked up by
//! `apply_live_runtime_config` on a tick, not at the call.
//!
//! The tick is `1000 / expiry.hz` ms — but it runs on a reactor that may
//! be parked or descheduled, so "how long since the work finished" is not
//! a number a test can write down. Two cells wrote one down anyway and
//! failed on a loaded machine: `tier_hydration` read 26 preads where 52
//! rows were cold (half the shards had published), and
//! `slowlog_hotreload` missed a config swap it had given 500 ms —
//! a margin its own comment says was copied from another file.
//!
//! `tier_hydration` already knew better one screen above the failure: its
//! `cold_keys` wait says "never a bare sleep on eviction timing" and polls
//! to a fixpoint. This module is that idiom, made shareable.
//!
//! Neither helper can turn a wrong answer into a right one. A gauge that
//! settles at the wrong value settles, and the assertion still fires on
//! it; for the `== 0` assertions, resting is strictly STRONGER than a
//! fixed sleep, because a late promotion has longer to show up.

#![allow(dead_code)] // each test binary uses a subset

use std::time::{Duration, Instant};

/// How long a value must hold still before it counts as settled. Several
/// tick intervals at the default `expiry.hz`, so every shard has had more
/// than one chance to publish.
pub const REST: Duration = Duration::from_millis(500);

/// How long to keep asking before giving up and saying what was seen.
pub const BUDGET: Duration = Duration::from_secs(20);

/// Read a whole SNAPSHOT until it stops changing for [`REST`].
///
/// Gauges that are compared to each other must come from ONE `INFO`
/// reply. `peek_preads_total` and `cold_keys` were read by two separate
/// round trips, so the equality between them was being asserted across two
/// different moments — a skew no amount of waiting removes, because each
/// read also refreshes the answering shard before summing the rest.
pub fn snapshot_at_rest<F: FnMut() -> Vec<u64>>(what: &str, mut read: F) -> Vec<u64> {
    let started = Instant::now();
    let mut last = read();
    let mut since = Instant::now();
    let mut reads = 1usize;
    while started.elapsed() < BUDGET {
        std::thread::sleep(Duration::from_millis(25));
        let now = read();
        reads += 1;
        if now == last {
            if since.elapsed() >= REST {
                return now;
            }
        } else {
            last = now;
            since = Instant::now();
        }
    }
    panic!("{what} never came to rest: still moving after {:?} and {reads} reads (last = {last:?})",
           started.elapsed());
}

/// Read until the value stops changing for [`REST`], then return it.
///
/// Panics — naming the last value and the number of reads — if it never
/// holds still inside [`BUDGET`]. That is a real failure: a gauge that
/// never settles is not a slow gauge, it is a moving one.
pub fn at_rest<F: FnMut() -> u64>(what: &str, mut read: F) -> u64 {
    let started = Instant::now();
    let mut last = read();
    let mut since = Instant::now();
    let mut reads = 1usize;
    while started.elapsed() < BUDGET {
        std::thread::sleep(Duration::from_millis(25));
        let now = read();
        reads += 1;
        if now == last {
            if since.elapsed() >= REST {
                return now;
            }
        } else {
            last = now;
            since = Instant::now();
        }
    }
    panic!("{what} never came to rest: still moving after {:?} and {reads} reads (last = {last})",
           started.elapsed());
}

/// Poll until `ok` returns true, or panic naming what was being waited for.
///
/// For an effect that either has landed or has not — a config swap picked
/// up, a listener gone — where the question is whether it happens at all,
/// not how many milliseconds it took on this machine.
pub fn until<F: FnMut() -> bool>(what: &str, mut ok: F) {
    let started = Instant::now();
    let mut polls = 0usize;
    while started.elapsed() < BUDGET {
        polls += 1;
        if ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("{what} never happened: {polls} polls over {:?}", started.elapsed());
}
