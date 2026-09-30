//! Packing a shard's index leaves on the reaper tick, the embedded
//! counterpart of the server's shard-tick repack: half a millisecond a
//! tick, a few leaves between clock reads, each segment resting once a
//! lap packs nothing until it has an eighth more leaves or an eighth fewer
//! entries.

use crate::ops_index::ShardSegs;

/// Time one shard's tick spends packing index leaves at most.
#[cfg(not(target_arch = "wasm32"))]
const TICK_BUDGET: std::time::Duration = std::time::Duration::from_micros(500);
/// Leaves one segment's step visits between clock reads.
const STEP_LEAVES: usize = 16;

/// One tick's packing on this shard.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn tick(segs: &mut ShardSegs) {
    let started = std::time::Instant::now();
    let mut busy = true;
    while busy && started.elapsed() < TICK_BUDGET {
        busy = false;
        for (_, seg) in &mut segs.segs {
            busy |= seg.tidy(STEP_LEAVES);
        }
    }
}

/// One tick's packing on this shard: without a clock, one step a segment.
#[cfg(target_arch = "wasm32")]
pub(crate) fn tick(segs: &mut ShardSegs) {
    for (_, seg) in &mut segs.segs {
        seg.tidy(STEP_LEAVES);
    }
}
