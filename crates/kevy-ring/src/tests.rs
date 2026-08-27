//! Unit tests for `kevy-ring`.
//!
//! Split out of `lib.rs` when that file reached the workspace's
//! 500-line ceiling — the same shape `kevy-uring/src/ring_tests.rs`
//! and `kevy-alloc/src/tests.rs` already take. Still a child module
//! of the crate root, so `use super::*` reaches everything private.

use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

#[test]
fn capacity_rounds_up_to_power_of_two() {
    let (tx, rx) = ring::<u8>(3);
    assert_eq!(tx.capacity(), 4);
    // Consumer-side capacity must report the same slot count as the
    // producer (both inspect the shared mask).
    assert_eq!(rx.capacity(), tx.capacity());
    let (tx, _rx) = ring::<u8>(1);
    assert_eq!(tx.capacity(), 2); // minimum
    let (tx, _rx) = ring::<u8>(1024);
    assert_eq!(tx.capacity(), 1024);
}

#[test]
fn fifo_order_and_full_empty() {
    let (mut tx, mut rx) = ring::<u32>(4); // 4 slots
    assert!(rx.is_empty());
    for i in 0..4 {
        assert!(tx.push(i).is_ok());
    }
    assert!(tx.is_full());
    assert_eq!(tx.push(99), Err(99)); // full → handed back
    for i in 0..4 {
        assert_eq!(rx.pop(), Some(i)); // FIFO
    }
    assert_eq!(rx.pop(), None);
    assert!(rx.is_empty());
}

#[test]
fn wraps_around_many_times() {
    // Push/pop far more than capacity to exercise index wrap.
    let (mut tx, mut rx) = ring::<usize>(2);
    for i in 0..10_000 {
        assert!(tx.push(i).is_ok());
        assert_eq!(rx.pop(), Some(i));
    }
    assert_eq!(rx.pop(), None);
}

#[test]
fn len_tracks_occupancy() {
    let (mut tx, mut rx) = ring::<u8>(8);
    assert_eq!(rx.len(), 0);
    tx.push(1).unwrap();
    tx.push(2).unwrap();
    assert_eq!(rx.len(), 2);
    rx.pop().unwrap();
    assert_eq!(rx.len(), 1);
}

use std::sync::Arc as StdArc;

// Drop-counting payload used by `drops_queued_elements_exactly_once`.
struct Bomb(StdArc<AtomicUsize>);
impl Drop for Bomb {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn drops_queued_elements_exactly_once() {
    // A payload that bumps a shared counter on drop; verify the ring's Drop
    // releases exactly the still-queued items (no leak, no double free).
    let dropped = StdArc::new(AtomicUsize::new(0));
    {
        let (mut tx, mut rx) = ring::<Bomb>(8);
        for _ in 0..5 {
            assert!(tx.push(Bomb(dropped.clone())).is_ok());
        }
        // Consume 2 (those drop now), leave 3 queued for the ring's Drop.
        drop(rx.pop());
        drop(rx.pop());
        assert_eq!(dropped.load(Ordering::SeqCst), 2);
        drop(tx);
        drop(rx); // last handle → Ring dropped → remaining 3 dropped
    }
    assert_eq!(dropped.load(Ordering::SeqCst), 5);
}

#[test]
fn spsc_stress_across_threads() {
    // Producer and consumer on separate threads; a small ring forces many
    // full/empty transitions. Every item must arrive exactly once, in order.
    // Miri interprets ~1000x slower than native; 2k iterations still cross
    // the full/empty boundary dozens of times, which is what the race
    // detector needs — large N only adds wall-clock, not coverage.
    const N: u64 = if cfg!(miri) { 2_000 } else { 1_000_000 };
    let (mut tx, mut rx) = ring::<u64>(64);
    let producer = std::thread::spawn(move || {
        for i in 0..N {
            while tx.push(i).is_err() {
                std::hint::spin_loop();
            }
        }
    });
    let mut next = 0u64;
    while next < N {
        match rx.pop() {
            Some(v) => {
                assert_eq!(v, next, "out-of-order or lost value");
                next += 1;
            }
            None => std::hint::spin_loop(),
        }
    }
    producer.join().unwrap();
    assert_eq!(next, N);
}

#[test]
fn stress_with_intermittent_consumer() {
    // Consumer occasionally stalls so the ring fills and the producer must
    // back off — exercises the full path under real contention.
    // Small N under miri for the same reason as `spsc_stress_across_threads`.
    const N: u64 = if cfg!(miri) { 2_000 } else { 200_000 };
    let (mut tx, mut rx) = ring::<u64>(16);
    let done = Arc::new(AtomicBool::new(false));
    let done_p = done.clone();
    let producer = std::thread::spawn(move || {
        for i in 0..N {
            while tx.push(i).is_err() {
                std::thread::yield_now();
            }
        }
        done_p.store(true, Ordering::Release);
    });
    let mut next = 0u64;
    let mut spins = 0u64;
    loop {
        if let Some(v) = rx.pop() {
            assert_eq!(v, next);
            next += 1;
            spins += 1;
            if spins.is_multiple_of(1000) {
                std::thread::yield_now(); // let the ring fill up
            }
        } else {
            if done.load(Ordering::Acquire) && rx.is_empty() {
                break;
            }
            std::thread::yield_now();
        }
    }
    producer.join().unwrap();
    assert_eq!(next, N);
}
