//! kevy-ring baseline: same-thread push+pop round-trip (raw op cost, no
//! coherency traffic) and cross-thread SPSC throughput (the cross-core
//! transport floor — head/tail cache lines bounce between two cores).

use kevy_bench::{bench, black_box};
use kevy_ring::ring;

const XTHREAD_ITEMS: u64 = 5_000_000;

pub fn run() {
    println!("== kevy-ring ==");

    let (mut p, mut c) = ring::<u64>(1024);
    let s = bench(50, 50_000, || {
        let _ = p.push(black_box(42u64));
        black_box(c.pop());
    });
    crate::row("push+pop u64 (same thread, cap=1024)", s, 1);

    // Cross-thread: each sample moves XTHREAD_ITEMS items through a fresh
    // ring with a fresh consumer thread; per-op = per-item transport cost.
    let s = bench(7, 1, || {
        let (mut p, mut c) = ring::<u64>(1024);
        let consumer = std::thread::spawn(move || {
            let mut got = 0u64;
            while got < XTHREAD_ITEMS {
                if c.pop().is_some() {
                    got += 1;
                }
            }
        });
        let mut sent = 0u64;
        while sent < XTHREAD_ITEMS {
            if p.push(sent).is_ok() {
                sent += 1;
            }
        }
        consumer.join().unwrap();
    });
    crate::row("cross-thread SPSC u64 (cap=1024)", s, XTHREAD_ITEMS as usize);
    println!();
}
