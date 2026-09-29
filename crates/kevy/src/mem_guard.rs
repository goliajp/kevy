//! Holding a tiered server's memory to its budget, from a thread of its own.
//!
//! Demotion holds `used_memory` plus the index floor to the budget, and
//! the process holds more than that. Two gaps, handled separately:
//!
//! - **Live memory the store does not account for** — receive rings,
//!   connection buffers, allocator overhead, an index holding more than it
//!   reports. Once a second, while RSS is past half the budget, the
//!   guard asks the allocator what is live and hands every shard its share
//!   of the difference, which lowers the demote target by that much.
//! - **Freed memory the allocator keeps** — a demoted value's blocks go to
//!   glibc's free lists and stay resident. When the allocator keeps more
//!   than 2% of the budget resident beyond what is live, the guard asks it
//!   to hand whole free pages back.
//!
//! If live memory stays past budget × 1.05 anyway — demotion has nothing
//! left to demote — every shard refuses growing writes until it falls back.
//!
//! The allocator walk and the trim both lock the heap's arenas while they
//! run (milliseconds to tens of milliseconds on a fragmented heap), so they
//! run here rather than on a shard, and only when RSS says they are due.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use crate::state::RuntimeState;

/// The reply to a growing write while the process is over its line.
pub(crate) const OVER_BUDGET_ERR: &str =
    "OOM command not allowed when the process holds more memory than the tiering budget allows";

/// How often the guard reads RSS.
const PERIOD: Duration = Duration::from_millis(100);
/// Reads between two allocator walks.
const WALK_EVERY: u32 = 10;

/// What the guard found, for `INFO` and for every shard's tick.
#[derive(Debug, Default)]
pub(crate) struct MemGuard {
    /// Growing writes are refused: live memory stayed past budget × 1.05.
    pub(crate) refusing: AtomicBool,
    /// Live bytes the store does not account for, spread over the shards.
    pub(crate) overhead_bytes: AtomicU64,
    /// Live bytes at the last walk: the allocator's in-use plus the
    /// keyspace tables mapped outside it.
    pub(crate) live_bytes: AtomicU64,
    /// Allocator walks, and the time they took (µs).
    pub(crate) walks: AtomicU64,
    pub(crate) walk_us: AtomicU64,
    /// Heap trims, the RSS they gave back, and the time they took (µs).
    pub(crate) trims: AtomicU64,
    pub(crate) trimmed_bytes: AtomicU64,
    pub(crate) trim_us: AtomicU64,
}

/// The RSS line: budget × 1.05.
#[inline]
pub(crate) fn rss_line(budget: u64) -> u64 {
    budget.saturating_add(budget / 20)
}

/// Start the guard when tiering is on. It holds the state weakly and ends
/// when the server does.
pub(crate) fn spawn_if_tiered(state: &Arc<RuntimeState>) {
    if state.config().tiering.budget.is_none() {
        return;
    }
    let weak = Arc::downgrade(state);
    std::thread::Builder::new()
        .name("kevy-memguard".into())
        .spawn(move || run(&weak))
        .expect("spawn the memory guard thread");
}

/// The guard's memory between looks.
#[derive(Debug, Default)]
struct Pace {
    /// Freed bytes the last trim could not hand back: the next trim waits
    /// until the allocator keeps 1% of budget more than that.
    kept_floor: u64,
    /// Looks since the last walk.
    since_walk: u32,
    /// Walks in a row that found live memory past the line.
    over: u32,
}

fn run(state: &Weak<RuntimeState>) {
    let mut pace = Pace { since_walk: WALK_EVERY, ..Pace::default() };
    loop {
        std::thread::sleep(PERIOD);
        let Some(state) = state.upgrade() else { return };
        let Ok(Some(budget)) = crate::resolve_tier_budget(&state.config()) else { continue };
        let t = state.obs.aggregate();
        look(&state.mem, budget, t.used_memory + t.tier.reserved_bytes, &mut pace);
    }
}

/// One look. `accounted` is what the shards charge: `used_memory` plus
/// the index floor.
fn look(g: &MemGuard, budget: u64, accounted: u64, pace: &mut Pace) {
    let rss = kevy_sys::process_rss_bytes();
    pace.since_walk += 1;
    // under half the budget nothing needs the walk; its last answer stands, and
    // the first look past it walks at once, before the hot set fills the rest
    if rss <= budget / 2 {
        pace.over = 0;
        g.refusing.store(false, Relaxed);
        return;
    }
    if pace.since_walk < WALK_EVERY {
        return;
    }
    pace.since_walk = 0;
    let t0 = Instant::now();
    let live = live_bytes();
    g.walk_us.fetch_add(t0.elapsed().as_micros() as u64, Relaxed);
    g.walks.fetch_add(1, Relaxed);
    g.live_bytes.store(live, Relaxed);
    g.overhead_bytes.store(live.saturating_sub(accounted), Relaxed);
    trim_if_kept(g, budget, rss.saturating_sub(live), pace);
    // one walk past the line can be a table mid-growth or a demotion
    // batch still catching up; two in a row, a second apart, is not
    pace.over = if live > rss_line(budget) { pace.over + 1 } else { 0 };
    g.refusing.store(pace.over >= 2, Relaxed);
}

/// Hand freed pages back once the allocator keeps more than 2% of the
/// budget resident beyond what is live, and again whenever that grows by
/// 1% past what the last trim could not return. Freed blocks that new
/// allocations take back are not kept any more, so the mark follows the
/// kept bytes down.
fn trim_if_kept(g: &MemGuard, budget: u64, kept: u64, pace: &mut Pace) {
    pace.kept_floor = pace.kept_floor.min(kept);
    if kept <= budget / 50 || kept <= pace.kept_floor.saturating_add(budget / 100) {
        return;
    }
    let before = kevy_sys::process_rss_bytes();
    let t0 = Instant::now();
    kevy_sys::malloc_trim_now();
    g.trim_us.fetch_add(t0.elapsed().as_micros() as u64, Relaxed);
    let given_back = before.saturating_sub(kevy_sys::process_rss_bytes());
    g.trims.fetch_add(1, Relaxed);
    g.trimmed_bytes.fetch_add(given_back, Relaxed);
    pace.kept_floor = kept.saturating_sub(given_back);
}

/// What the process holds live: the allocator's in-use and the keyspace
/// tables mapped outside it. RSS when the allocator publishes nothing.
fn live_bytes() -> u64 {
    match kevy_sys::heap_stats() {
        Some(h) => h.in_use + kevy_madvise::mapped_bytes() as u64,
        None => kevy_sys::process_rss_bytes(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_process_far_under_its_budget_is_left_alone() {
        let g = MemGuard::default();
        let mut pace = Pace::default();
        for _ in 0..WALK_EVERY * 2 {
            look(&g, u64::MAX / 2, 0, &mut pace);
        }
        assert!(!g.refusing.load(Relaxed));
        assert_eq!(g.trims.load(Relaxed) + g.walks.load(Relaxed), 0, "no trim, no walk");
    }

    #[test]
    fn live_memory_past_the_line_twice_in_a_row_is_refused() {
        // a budget far below what this test process already holds live
        let g = MemGuard::default();
        let mut pace = Pace::default();
        for _ in 0..WALK_EVERY {
            look(&g, 1, 0, &mut pace);
        }
        assert_eq!(g.walks.load(Relaxed), 1);
        assert!(!g.refusing.load(Relaxed), "one walk over the line is not enough");
        let live = g.live_bytes.load(Relaxed);
        assert!(live > 0 && g.overhead_bytes.load(Relaxed) == live, "nothing accounted");
        for _ in 0..WALK_EVERY {
            look(&g, 1, 0, &mut pace);
        }
        assert!(g.refusing.load(Relaxed), "two walks over the line refuse");
    }

    #[test]
    fn the_overhead_is_what_the_store_does_not_account_for() {
        let g = MemGuard::default();
        let mut pace = Pace { since_walk: WALK_EVERY, ..Pace::default() };
        look(&g, 1, u64::MAX, &mut pace);
        assert_eq!(g.overhead_bytes.load(Relaxed), 0, "everything live is accounted");
    }
}
