//! kevy-alloc baseline, measured beside the system allocator on the same
//! shapes.
//!
//! The comparison is here to say where we stand, not as a target — the
//! criterion that decides this crate is the accounting identity and the
//! reclaim property, not a nanosecond count. What these rows *do* guard
//! is the risk that sinks the whole train: an allocator has no off
//! switch, so a fast path materially slower than the system one shows up
//! on every SET, GET and published message.
//!
//! 400 bytes appears because that is the value size at which resident
//! memory ran 2.24× the logical bound in the PostgreSQL comparison.

use kevy_alloc::Heap;
use kevy_bench::{bench, black_box};

use std::alloc::{Layout, alloc as sys_alloc, dealloc as sys_dealloc};

const CHURN: usize = 4096;

pub fn run() {
    println!("== kevy-alloc ==");
    if !kevy_alloc::os::available() {
        println!("  (skipped: no anonymous mapping on this target)");
        return;
    }

    for size in [64usize, 400, 4096] {
        round_trip(size);
    }
    churn(400);
    churn_system(400);
    reclaim_cost(400);
}

/// Allocate and immediately free — the hot path, hitting the current
/// span's free list every time.
fn round_trip(size: usize) {
    let mut heap = Heap::new(0);
    // Warm the span so the first sample is not paying for a map syscall.
    if let Some(p) = heap.alloc(size, 8) {
        unsafe { heap.dealloc(p, size, 8) };
    }
    let s = bench(30, 20_000, || {
        if let Some(p) = heap.alloc(black_box(size), 8) {
            unsafe { heap.dealloc(p, size, 8) };
        }
    });
    crate::row(&format!("alloc+free {size}B (kevy-alloc)"), s, 1);

    let layout = Layout::from_size_align(size, 8).unwrap();
    let s = bench(30, 20_000, || {
        // SAFETY: non-zero layout; freed immediately with the same one.
        unsafe {
            let p = sys_alloc(layout);
            if !p.is_null() {
                sys_dealloc(black_box(p), layout);
            }
        }
    });
    crate::row(&format!("alloc+free {size}B (system)"), s, 1);
}

/// Hold many live at once, then release them in an order unrelated to
/// allocation — the shape that strands pages under glibc.
fn churn(size: usize) {
    let mut heap = Heap::new(0);
    let s = bench(10, 20, || {
        let mut live: Vec<_> = (0..CHURN).filter_map(|_| heap.alloc(size, 8)).collect();
        // Free every other one first, then the rest: adjacent slots go
        // back at different times, which is what fragments a bump heap.
        for i in (0..live.len()).step_by(2) {
            unsafe { heap.dealloc(live[i], size, 8) };
        }
        for i in (1..live.len()).step_by(2) {
            unsafe { heap.dealloc(live[i], size, 8) };
        }
        live.clear();
    });
    crate::row(
        &format!("churn {CHURN}x{size}B interleaved free (kevy-alloc)"),
        s,
        CHURN,
    );
}

/// What returning pages actually costs, and what an idle sweep costs.
///
/// Priced apart from the churn rows because the system allocator has no
/// counterpart to compare against: folding page-return into the churn
/// row timed our reclaim against their nothing, which read as a 1.5×
/// loss that was really a missing column. Against the 4 ns churn row
/// above, the difference here is the price of the property glibc cannot
/// offer at any price.
///
/// The second row is the sweep finding nothing to do — the cost of
/// running reclaim on a tick. It is much cheaper because spans already
/// discarded are skipped, which is why measuring only the repeat call
/// would have flattered us.
fn reclaim_cost(size: usize) {
    let mut heap = Heap::new(0);
    let s = bench(10, 20, || {
        let live: Vec<_> = (0..CHURN).filter_map(|_| heap.alloc(size, 8)).collect();
        for p in &live {
            unsafe { heap.dealloc(*p, size, 8) };
        }
        heap.reclaim();
    });
    crate::row(
        &format!("churn {CHURN}x{size}B + page return (kevy-alloc)"),
        s,
        CHURN,
    );

    let s = bench(20, 200, || heap.reclaim());
    crate::row("reclaim sweep, nothing to return (per call)", s, 1);
}

/// The same shape through the system allocator, for scale.
fn churn_system(size: usize) {
    let layout = Layout::from_size_align(size, 8).unwrap();
    let s = bench(10, 20, || {
        // SAFETY: non-zero layout; every pointer is freed once below
        // with the layout it was made with.
        unsafe {
            let live: Vec<_> = (0..CHURN).map(|_| sys_alloc(layout)).collect();
            for i in (0..live.len()).step_by(2) {
                sys_dealloc(live[i], layout);
            }
            for i in (1..live.len()).step_by(2) {
                sys_dealloc(live[i], layout);
            }
        }
    });
    crate::row(
        &format!("churn {CHURN}x{size}B interleaved free (system)"),
        s,
        CHURN,
    );
}
