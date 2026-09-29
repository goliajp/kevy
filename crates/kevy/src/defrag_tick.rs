//! Running the store's defrag pass on the shard tick, when kevy-alloc is
//! the process's allocator.
//!
//! A demoted row leaves a hole wherever it sat, and a page goes back to
//! the OS only once nothing on it is live, so a tiered server that demotes
//! a fifth of its rows keeps most of their pages. kevy-alloc knows which
//! of its spans are sparser than their class; the store knows who owns
//! each value; this tick puts the two together: while the free space the
//! allocator holds inside its spans is large next to what is live, the
//! shard copies the values the allocator names into denser spans, and the
//! reclaim that follows in the same tick hands back what emptied.
//!
//! Paced like demotion — half a millisecond a tick at most — and with a
//! hysteresis band so a heap does not start and stop on the line. A lap
//! of the table that moves nothing ends the pass until the free space has
//! grown by a quarter again: what is left then is not in values the store
//! can move (an index leaf, a large collection).

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::{Duration, Instant};

use kevy_store::Store;

/// Set once by the server binary where it installs kevy-alloc as the
/// global allocator. Linking kevy-alloc (the feature) does not make it the
/// allocator, and its hint means nothing about another allocator's memory.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Time one shard tick may spend copying values.
const TICK_BUDGET: Duration = Duration::from_micros(500);
/// Buckets of the table one step walks between clock reads.
const STEP_BUCKETS: usize = 256;

/// Record that kevy-alloc is the process's global allocator, so shards
/// started after this defragment their heaps. Called by the server binary
/// right beside its `#[global_allocator]`; a program that installs another
/// allocator must not call it.
///
/// ```
/// // a program whose `#[global_allocator]` is `kevy_alloc::KevyAlloc`:
/// kevy::kevy_alloc_is_global();
/// ```
pub fn kevy_alloc_is_global() {
    ACTIVE.store(true, Relaxed);
}

/// Give a shard's store the allocator's hint, when there is one to give.
pub(crate) fn install(store: &mut Store) {
    #[cfg(feature = "kevy-alloc")]
    if ACTIVE.load(Relaxed) {
        store.set_defrag_hint(Some(hint));
    }
    #[cfg(not(feature = "kevy-alloc"))]
    let _ = store;
}

#[cfg(feature = "kevy-alloc")]
fn hint(ptr: *const u8, size: usize, align: usize) -> bool {
    std::alloc::Layout::from_size_align(size, align)
        .is_ok_and(|l| kevy_alloc::global::should_move(ptr, l))
}

/// A shard's pass: running or not, and the free space a fruitless lap
/// stopped at.
#[derive(Clone, Copy, Default)]
struct Pace {
    running: bool,
    moved_this_lap: usize,
    parked_at: u64,
}

thread_local! {
    static PACE: Cell<Pace> = const { Cell::new(Pace { running: false, moved_this_lap: 0, parked_at: 0 }) };
}

/// Whether free space `free` beside `held` bytes calls for a pass: start
/// above 1/32 (and 4 MiB), stop below 1/128 (and 1 MiB).
fn wanted(p: Pace, free: u64, held: u64) -> bool {
    let (start, stop) = ((held / 32).max(4 << 20), (held / 128).max(1 << 20));
    if p.running {
        return free > stop;
    }
    free > start && free > p.parked_at + p.parked_at / 4
}

/// One tick's pass on this shard.
pub(crate) fn tick(store: &mut Store) {
    #[cfg(feature = "kevy-alloc")]
    {
        if !ACTIVE.load(Relaxed) {
            return;
        }
        let Some(a) = kevy_alloc::thread_stats() else { return };
        run(store, a.span_free, a.live + a.rounding);
    }
    #[cfg(not(feature = "kevy-alloc"))]
    let _ = store;
}

#[cfg_attr(not(feature = "kevy-alloc"), allow(dead_code))]
fn run(store: &mut Store, free: u64, held: u64) {
    let mut p = PACE.with(Cell::get);
    p.running = wanted(p, free, held);
    if p.running {
        let started = Instant::now();
        loop {
            let step = store.defrag_step(STEP_BUCKETS);
            p.moved_this_lap += step.moved;
            if step.lap_done {
                if p.moved_this_lap == 0 {
                    (p.running, p.parked_at) = (false, free);
                    break;
                }
                p.moved_this_lap = 0;
            }
            if started.elapsed() >= TICK_BUDGET {
                break;
            }
        }
    } else {
        p.moved_this_lap = 0;
    }
    PACE.with(|c| c.set(p));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pass_starts_above_the_band_stops_below_it_and_waits_after_a_dry_lap() {
        let held = 1u64 << 30;
        let idle = Pace::default();
        assert!(!wanted(idle, held / 40, held), "inside the band: not started");
        assert!(wanted(idle, held / 20, held), "above 1/32: starts");
        let on = Pace { running: true, ..Pace::default() };
        assert!(wanted(on, held / 40, held), "inside the band: keeps going");
        assert!(!wanted(on, held / 200, held), "below 1/128: stops");
        let parked = Pace { parked_at: held / 20, ..Pace::default() };
        assert!(!wanted(parked, held / 20 + held / 100, held), "a dry lap waits for growth");
        assert!(wanted(parked, held / 10, held), "and resumes once free space grew by a quarter");
    }
}
