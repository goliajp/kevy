use core::ptr::NonNull;
use std::collections::HashMap;

use crate::class;
use crate::heap::Heap;
use crate::os::{self, PAGE};
use crate::pagemap::{NO_CLASS, PAGES_PER_SPAN, slots_of_page};
use crate::segment::{self, FIRST_DATA_SPAN, SPANS_PER_SEGMENT};

use super::PURGE_DELAY;

const SIZES: [usize; 8] = [16, 24, 100, 400, 912, 4_000, 20_000, 32_768];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn agree(h: &Heap, what: &str) {
    let (fast, walked) = (h.snapshot(), h.snapshot_walked());
    assert_eq!(fast, walked, "after {what}");
    assert!(fast.balanced(), "after {what}: {fast:?}");
}

/// Sweep both heaps, `b` first so its outbound frees reach `a` before
/// `a` sweeps, and check the totals against the walk.
fn tick(a: &mut Heap, b: &mut Heap) {
    b.reclaim_with(true);
    a.reclaim_with(true);
    agree(a, "a's sweep");
    agree(b, "b's sweep");
    assert_eq!(a.snapshot().cache, 0, "a foreign free shipped home was not drained");
    nothing_old_held(a);
    nothing_old_held(b);
}

/// A shard's steady state: a fixed set of long-lived values, and per
/// tick the same request-shaped allocations of every size, half of them
/// outliving the tick and some freed by the other shard. Returns the
/// pages `a` gave back after warm-up.
fn steady(delay: u32, ticks: usize) -> u64 {
    let (mut a, mut b) = (Heap::new(1), Heap::new(2));
    (a.purge_delay, b.purge_delay) = (delay, delay);
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let base: Vec<_> = (0..3_000)
        .map(|i| {
            let size = SIZES[i % SIZES.len()];
            (a.alloc(size, 8).expect("mapped"), size)
        })
        .collect();
    let mut carried: Vec<(NonNull<u8>, usize)> = Vec::new();
    let mut used: HashMap<usize, u32> = HashMap::new();
    let mut warm = 0;
    for t in 0..ticks {
        let mut fresh: Vec<(NonNull<u8>, usize)> = Vec::new();
        for &size in &SIZES {
            for _ in 0..40 {
                let p = a.alloc(size, 8).expect("mapped");
                let start = p.as_ptr() as usize & !(PAGE - 1);
                for page in (start..p.as_ptr() as usize + size).step_by(PAGE) {
                    used.insert(page, a.epoch);
                }
                fresh.push((p, size));
            }
        }
        for (p, size) in carried.drain(..) {
            // SAFETY: allocated by `a` with this size.
            unsafe { a.dealloc(p, size, 8) };
        }
        // every other one outlives the tick, the same ones each time;
        // the rest go in a random order
        let (keep, mut gone): (Vec<_>, Vec<_>) =
            fresh.into_iter().enumerate().partition(|(i, _)| i % 2 == 0);
        carried = keep.into_iter().map(|(_, x)| x).collect();
        for i in (1..gone.len()).rev() {
            gone.swap(i, rng.next() as usize % (i + 1));
        }
        for (i, (_, (p, size))) in gone.into_iter().enumerate() {
            // SAFETY: allocated by `a`; `b` ships every fifth one home.
            unsafe { if i % 5 == 0 { b.dealloc(p, size, 8) } else { a.dealloc(p, size, 8) } }
        }
        tick(&mut a, &mut b);
        nothing_young_returned(&a, &used);
        if t == delay as usize + 2 {
            warm = a.discards;
        }
    }
    for (p, size) in carried.into_iter().chain(base) {
        // SAFETY: allocated by `a` with this size.
        unsafe { a.dealloc(p, size, 8) };
    }
    a.discards - warm
}

/// Request buffers reuse the same pages every tick. Returning them at
/// each sweep is what the next tick paid for in faults.
#[test]
fn a_steady_working_set_returns_no_page_after_warm_up() {
    if !os::available() {
        return;
    }
    assert_eq!(steady(PURGE_DELAY, 40), 0);
}

/// The control that shows the test above can fail: without the delay
/// the same traffic has pages returned on every tick.
#[test]
fn without_the_delay_the_same_traffic_returns_pages_every_tick() {
    if !os::available() {
        return;
    }
    assert!(steady(0, 40) >= 25, "the steady test cannot tell the delay from none");
}

/// Every page an allocation landed on within the last `delay` sweeps is
/// still resident and its span still has its class.
fn nothing_young_returned(h: &Heap, used: &HashMap<usize, u32>) {
    for (&page, &at) in used {
        if h.epoch.wrapping_sub(at) > h.purge_delay {
            continue;
        }
        // SAFETY: an address inside a live segment of `h`.
        let seg = unsafe { segment::segment_of(NonNull::new_unchecked(page as *mut u8)).as_ref() };
        let ix = (page & (segment::SEGMENT_BYTES - 1)) / class::SPAN_BYTES;
        let p = (page & (class::SPAN_BYTES - 1)) / PAGE;
        let m = &seg.spans()[ix];
        assert_ne!(m.class(), NO_CLASS, "span of a page used at {at} retired at {}", h.epoch);
        assert_eq!(m.discarded() & (1 << p), 0, "page used at {at} returned at {}", h.epoch);
    }
}

/// The walk the wheel is judged against: every assigned empty span, and
/// every resident free touched page of a live span, is one `may_hold`
/// accepts, given the age of the span's youngest page or of the page.
fn held_only(h: &Heap, may_hold: impl Fn(u32) -> bool) {
    let mut seg = h.segments;
    while !seg.is_null() {
        // SAFETY: the heap's own segment list.
        let s = unsafe { &*seg };
        for ix in FIRST_DATA_SPAN..SPANS_PER_SEGMENT {
            let m = &s.spans[ix];
            if m.class == NO_CLASS {
                continue;
            }
            let slot = class::size_of(m.class as usize);
            let touched = (usize::from(m.high_water) * slot).div_ceil(PAGE);
            let age = |p: usize| h.epoch.wrapping_sub(s.stamps[ix].0[p]);
            if m.live == 0 {
                let youngest = (0..touched).map(age).min().unwrap_or(u32::MAX);
                assert!(may_hold(youngest), "span {ix} held empty at age {youngest}");
                continue;
            }
            for p in 0..PAGES_PER_SPAN {
                let (lo, hi) = slots_of_page(p, slot, m.capacity());
                let free = p < touched && !m.range_has_live(lo, hi) && m.discarded & (1 << p) == 0;
                assert!(!free || may_hold(age(p)), "span {ix} page {p} held at age {}", age(p));
            }
        }
        seg = s.next;
    }
}

/// Nothing past the delay is still held: the wheel visited every span
/// that had something due.
fn nothing_old_held(h: &Heap) {
    held_only(h, |age| age <= h.purge_delay);
}

/// Random churn, then none: at every sweep no recently used page has
/// gone, and `delay + 1` sweeps after the last allocation everything
/// free has.
fn decay(seed: u64, delay: u32) {
    let (mut a, mut b) = (Heap::new(1), Heap::new(2));
    (a.purge_delay, b.purge_delay) = (delay, delay);
    let mut rng = Rng(seed);
    let mut held: Vec<(NonNull<u8>, usize)> = Vec::new();
    let mut used: HashMap<usize, u32> = HashMap::new();
    for _ in 0..30 {
        for _ in 0..(rng.next() % 400) {
            let r = rng.next();
            if !r.is_multiple_of(3) || held.is_empty() {
                let size = SIZES[(r >> 8) as usize % SIZES.len()] - (r >> 16) as usize % 8;
                let p = a.alloc(size, 8).expect("mapped");
                let start = p.as_ptr() as usize & !(PAGE - 1);
                for page in (start..p.as_ptr() as usize + size).step_by(PAGE) {
                    used.insert(page, a.epoch);
                }
                held.push((p, size));
            } else {
                let (p, size) = held.swap_remove((r >> 8) as usize % held.len());
                // SAFETY: allocated by `a`; `b` ships some of them home.
                unsafe {
                    if r.is_multiple_of(7) { b.dealloc(p, size, 8) } else { a.dealloc(p, size, 8) }
                }
            }
        }
        tick(&mut a, &mut b);
        nothing_young_returned(&a, &used);
    }
    // the last allocations were stamped one sweep back; a third of what
    // is held goes now, so the frees need one sweep of their own
    let bound = a.epoch + delay.max(1);
    for (i, (p, size)) in held.into_iter().enumerate() {
        if i % 3 == 0 {
            // SAFETY: allocated by `a` with this size.
            unsafe { a.dealloc(p, size, 8) };
        }
    }
    while a.epoch < bound {
        tick(&mut a, &mut b);
        nothing_young_returned(&a, &used);
    }
    held_only(&a, |_| false);
}

#[test]
fn once_churn_stops_everything_free_goes_within_the_delay_and_nothing_sooner() {
    if !os::available() {
        return;
    }
    for seed in [0x9E37_79B9_7F4A_7C15, 0xD1B5_4A32_D192_ED03, 0x2545_F491_4F6C_DD1D, 7] {
        for delay in [0, 1, 3, PURGE_DELAY] {
            decay(seed, delay);
        }
    }
}

/// Once everything freed has aged out, a sweep has no span to visit and
/// no segment to drain, however large the heap.
#[test]
fn a_settled_heap_leaves_the_sweep_nothing_to_visit() {
    if !os::available() {
        return;
    }
    let (mut a, mut b) = (Heap::new(1), Heap::new(2));
    let held: Vec<_> = (0..8_000).map(|_| a.alloc(4_000, 8).expect("mapped")).collect();
    for (i, p) in held.iter().enumerate() {
        if i % 2 == 0 {
            // SAFETY: allocated by `a` with this size; `b` ships some home.
            unsafe { if i % 4 == 0 { b.dealloc(*p, 4_000, 8) } else { a.dealloc(*p, 4_000, 8) } }
        }
    }
    assert!(a.tally.segments > 4, "the heap should span several segments");
    for _ in 0..=PURGE_DELAY + 1 {
        tick(&mut a, &mut b);
    }
    assert!(a.wheel.iter().all(|&r| r == 0), "spans still scheduled after the delay");
    let mut queued = 0;
    // SAFETY: the first segment's tally, mapped while `a` lives.
    unsafe { &*a.parked }.drain_pending(|_, _| queued += 1);
    assert_eq!(queued, 0, "a segment stayed queued with nothing to drain");
}
