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
//! Paced like demotion — half a millisecond a tick, rising to two while
//! the free space is past a sixteenth of what is live, so that the holes a
//! burst of demotion leaves are packed before the next allocation spike
//! lands on top of them — and with a hysteresis band so a heap does not
//! start and stop on the line. A lap
//! of the table that moves nothing ends the pass until the free space has
//! grown by a quarter again: what is left then is not in values the store
//! can move (an index leaf, a large collection).

// the pacing is exercised by its tests either way, and driven only by a
// build that links kevy-alloc
#![cfg_attr(not(feature = "kevy-alloc"), allow(dead_code))]

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::{Duration, Instant};

use kevy_store::Store;

/// Set once by the server binary where it installs kevy-alloc as the
/// global allocator. Linking kevy-alloc (the feature) does not make it the
/// allocator, and its hint means nothing about another allocator's memory.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Time one shard tick spends copying values at the band's edge, and at
/// most, once the free space reaches a sixteenth of what is live. On D1 a
/// keyspace table's doubling followed the demotion that made room for it
/// by three seconds; at the base rate the holes took five to pack, and the
/// doubling landed on them.
const TICK_BUDGET: Duration = Duration::from_micros(500);
const TICK_BUDGET_MAX: Duration = Duration::from_millis(2);
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

/// Whether kevy-alloc is this process's allocator. The feature only links
/// it: a program using this library under its own allocator gets `false`
/// here in a build that has the feature on.
pub(crate) fn active() -> bool {
    cfg!(feature = "kevy-alloc") && ACTIVE.load(Relaxed)
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
/// above 1/64 (and 4 MiB), stop below 1/256 (and 1 MiB).
fn wanted(p: Pace, free: u64, held: u64) -> bool {
    let (start, stop) = ((held / 64).max(4 << 20), (held / 256).max(1 << 20));
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

/// The tick's time for a pass: the base up to 1/32 of `held` free, rising
/// in step with the free share to the most at 1/16.
fn budget(free: u64, held: u64) -> Duration {
    let (lo, hi) = (held / 32, held / 16);
    if free <= lo || hi <= lo {
        return if free > lo { TICK_BUDGET_MAX } else { TICK_BUDGET };
    }
    let t = (free.min(hi) - lo) as f64 / (hi - lo) as f64;
    TICK_BUDGET + (TICK_BUDGET_MAX - TICK_BUDGET).mul_f64(t)
}

fn run(store: &mut Store, free: u64, held: u64) {
    let mut p = PACE.with(Cell::get);
    p.running = wanted(p, free, held);
    if p.running {
        let (started, allowed) = (Instant::now(), budget(free, held));
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
            if started.elapsed() >= allowed {
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
        assert!(!wanted(idle, held / 80, held), "inside the band: not started");
        assert!(wanted(idle, held / 40, held), "above 1/64: starts");
        let on = Pace { running: true, ..Pace::default() };
        assert!(wanted(on, held / 80, held), "inside the band: keeps going");
        assert!(!wanted(on, held / 400, held), "below 1/256: stops");
        let parked = Pace { parked_at: held / 20, ..Pace::default() };
        assert!(!wanted(parked, held / 20 + held / 100, held), "a dry lap waits for growth");
        assert!(wanted(parked, held / 10, held), "and resumes once free space grew by a quarter");
    }

    #[test]
    fn the_budget_climbs_from_the_base_to_the_most_across_a_thirty_second_of_live() {
        let held = 1u64 << 30;
        assert_eq!(budget(held / 64, held), TICK_BUDGET);
        assert_eq!(budget(held / 8, held), TICK_BUDGET_MAX);
        let mid = budget(held * 3 / 64, held);
        assert!(mid > TICK_BUDGET && mid < TICK_BUDGET_MAX, "{mid:?}");
        assert_eq!(budget(1, 0), TICK_BUDGET_MAX, "no live bytes: any free space gets the most");
        assert_eq!(budget(0, 0), TICK_BUDGET);
    }

    fn pace() -> Pace {
        PACE.with(Cell::get)
    }

    fn store_with_one_value() -> Store {
        let mut s = Store::new();
        s.set_slice(b"k", &[7u8; 64], None, kevy_store::SetCondition::Always);
        s
    }

    #[test]
    fn a_pass_below_the_band_does_not_touch_the_table() {
        PACE.with(|c| c.set(Pace { moved_this_lap: 3, ..Pace::default() }));
        let mut s = store_with_one_value();
        s.set_defrag_hint(Some(|_, _, _| true));
        run(&mut s, 0, 1 << 30);
        let p = pace();
        assert!(!p.running);
        assert_eq!(p.moved_this_lap, 0, "an idle tick forgets a half-done lap");
    }

    #[test]
    fn a_lap_that_moves_nothing_parks_the_pass_at_the_free_space_it_saw() {
        PACE.with(|c| c.set(Pace::default()));
        let mut s = store_with_one_value();
        s.set_defrag_hint(Some(|_, _, _| false));
        let free = 64 << 20;
        run(&mut s, free, 1 << 30);
        let p = pace();
        assert!(!p.running, "a dry lap ends the pass");
        assert_eq!(p.parked_at, free);
        run(&mut s, free, 1 << 30);
        assert!(!pace().running, "and it stays parked until the free space grows");
    }

    #[test]
    fn a_pass_that_keeps_moving_runs_until_its_tick_budget_is_spent() {
        PACE.with(|c| c.set(Pace::default()));
        // a table wider than one step, so steps end mid-lap as well
        let mut s = store_with_one_value();
        for i in 0..2_000u32 {
            s.set_slice(&i.to_be_bytes(), &[1u8; 64], None, kevy_store::SetCondition::Always);
        }
        s.set_defrag_hint(Some(|_, _, _| true));
        let started = Instant::now();
        run(&mut s, 64 << 20, 1 << 30);
        assert!(started.elapsed() >= TICK_BUDGET, "stopped before its budget");
        let p = pace();
        assert!(p.running, "a lap that moved something keeps the pass going");
        assert_eq!(s.get(b"k").unwrap().as_deref(), Some(&[7u8; 64][..]), "moving keeps the value");
    }

    #[test]
    fn a_lap_that_moved_something_starts_the_next_lap_from_nothing() {
        PACE.with(|c| c.set(Pace { moved_this_lap: 5, ..Pace::default() }));
        // one value: every step is a whole lap, and every lap moves it
        let mut s = store_with_one_value();
        s.set_defrag_hint(Some(|_, _, _| true));
        run(&mut s, 64 << 20, 1 << 30);
        let p = pace();
        assert!(p.running, "a lap that moved something keeps the pass going");
        assert_eq!(p.moved_this_lap, 0, "each finished lap starts its count over");
        assert_eq!(s.get(b"k").unwrap().as_deref(), Some(&[7u8; 64][..]));
    }

    // with the feature on, lib tests run under the system allocator, where
    // the flag would hand shards a hint about memory kevy-alloc does not own
    #[cfg(not(feature = "kevy-alloc"))]
    #[test]
    fn declaring_kevy_alloc_global_sets_the_flag_shards_read() {
        kevy_alloc_is_global();
        assert!(ACTIVE.load(Relaxed));
    }
}
