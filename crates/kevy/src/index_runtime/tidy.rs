//! Packing this shard's index leaves on the tick.
//!
//! A write splits a full leaf in two and never packs, so an index written
//! in random order holds leaves about 70% full, and one fed by several
//! shards at once lands anywhere between half and full. Paced like the
//! defrag tick: half a millisecond a tick, a few leaves between clock
//! reads, each segment resting once a lap packs nothing until its leaves
//! per entry grow by an eighth. The segment a tick starts with rotates, so
//! a large index being packed does not hold back the others.

use std::cell::Cell;
use std::time::{Duration, Instant};

use super::{BuildState, ShardIndexes};

/// Time one shard tick spends packing index leaves at most.
const TICK_BUDGET: Duration = Duration::from_micros(500);
/// Leaves one segment's step visits between clock reads.
const STEP_LEAVES: usize = 4;

thread_local! {
    static NEXT: Cell<usize> = const { Cell::new(0) };
}

/// One tick's packing on this shard.
pub(super) fn tick(st: &mut ShardIndexes) {
    let n = st.idx.len();
    if n == 0 {
        return;
    }
    let started = Instant::now();
    let first = NEXT.with(Cell::get) % n;
    let mut busy = true;
    while busy {
        busy = false;
        for i in (first..n).chain(0..first) {
            let si = &mut st.idx[i];
            if !matches!(si.build, BuildState::Ready) {
                continue;
            }
            let mut more = step(&mut si.seg);
            if let Some(g) = &mut si.global {
                for (_, seg) in &mut g.owned {
                    more |= step(seg);
                }
            }
            if more {
                (busy, st.stats_dirty) = (true, true);
            }
            if started.elapsed() >= TICK_BUDGET {
                NEXT.with(|c| c.set(i + 1));
                return;
            }
        }
    }
}

/// One segment's step of packing.
#[cfg(not(feature = "harness-repack-off"))]
#[inline(always)]
fn pack(seg: &mut kevy_index::Segment) -> bool {
    seg.tidy(STEP_LEAVES)
}

/// The harness build that measures the server without the repack: every
/// segment reads as having nothing to pack, so a tick walks the indexes
/// once and returns.
#[cfg(feature = "harness-repack-off")]
fn pack(_: &mut kevy_index::Segment) -> bool {
    let _ = STEP_LEAVES;
    false
}

#[cfg(not(feature = "harness-repack-trace"))]
use pack as step;
#[cfg(not(feature = "harness-repack-trace"))]
pub(super) use tick as run;

// The harness build that times every step and tick.
#[cfg(feature = "harness-repack-trace")]
#[path = "tidy_trace.rs"]
mod trace;
#[cfg(feature = "harness-repack-trace")]
pub(super) use trace::run;
#[cfg(feature = "harness-repack-trace")]
use trace::step;
