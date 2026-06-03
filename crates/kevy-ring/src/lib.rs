//! kevy-ring — a lock-free, bounded **single-producer / single-consumer** ring.
//!
//! One producer pushes, one consumer pops, with no locks and no per-message
//! allocation: a fixed power-of-two slot array plus two monotonic cursors. It is
//! the cross-core transport primitive for [kevy-rt]'s shared-nothing,
//! thread-per-core runtime (the Seastar/Scylla model) — each ordered pair of
//! cores gets its own ring, so a hot reactor never contends a lock on the hop.
//!
//! The single-producer / single-consumer contract is enforced *by the type
//! system*: [`push`](Producer::push) and [`pop`](Consumer::pop) take `&mut self`,
//! and [`Producer`]/[`Consumer`] are distinct owned halves, so the compiler
//! guarantees at most one thread pushes and one pops. That keeps the ordering
//! requirements minimal — a single `Release`/`Acquire` pair per operation.
//!
//! Pure Rust, zero dependencies. The lock-free buffer needs `UnsafeCell` +
//! atomics, so this crate is not `#![forbid(unsafe_code)]`; every `unsafe` block
//! documents the SPSC invariant it relies on (no C, no FFI — see the kevy
//! pure-Rust principle). Part of the [kevy] key–value server.
//!
//! [kevy]: https://crates.io/crates/kevy
//! [kevy-rt]: https://crates.io/crates/kevy-rt
//!
//! # Example
//!
//! ```
//! let (mut tx, mut rx) = kevy_ring::ring::<u32>(4);
//! assert!(tx.push(1).is_ok());
//! assert!(tx.push(2).is_ok());
//! assert_eq!(rx.pop(), Some(1));
//! assert_eq!(rx.pop(), Some(2));
//! assert_eq!(rx.pop(), None);
//! ```
//!
//! Producer and consumer move to different threads:
//!
//! ```
//! let (mut tx, mut rx) = kevy_ring::ring::<u64>(1024);
//! let prod = std::thread::spawn(move || {
//!     for i in 0..10_000u64 {
//!         while tx.push(i).is_err() {
//!             std::hint::spin_loop(); // ring full — let the consumer drain
//!         }
//!     }
//! });
//! let mut next = 0u64;
//! while next < 10_000 {
//!     if let Some(v) = rx.pop() {
//!         assert_eq!(v, next); // FIFO, nothing lost or reordered
//!         next += 1;
//!     }
//! }
//! prod.join().unwrap();
//! ```

#![warn(missing_docs)]
// `--cfg loom` is a known custom cfg used by tests/loom.rs to swap atomics +
// UnsafeCell + Arc for loom's instrumented versions. Tell rustc not to
// warn about the unrecognized cfg name on normal builds.
#![allow(unexpected_cfgs)]

// Loom-compat shim: under `--cfg loom` the model checker substitutes its own
// instrumented atomics + `UnsafeCell` + `Arc`, and `tests/loom.rs` enumerates
// every possible interleaving of the SPSC algorithm against this exact crate.
// Under a normal build the imports are the std originals — zero overhead.
//
// `UnsafeCellExt::with_mut` unifies the two APIs: loom's `UnsafeCell` only
// exposes `with_mut`; std's exposes `.get()`. We give std a tiny shim that
// delegates to `.get()` so the same call-site (`cell.with_mut(|p| ...)`) works
// in both modes. No production code change in the hot path.
mod sync {
    #[cfg(loom)]
    pub use loom::cell::UnsafeCell;
    #[cfg(loom)]
    pub use loom::sync::Arc;
    #[cfg(loom)]
    pub use loom::sync::atomic::{AtomicUsize, Ordering};

    #[cfg(not(loom))]
    pub use std::cell::UnsafeCell;
    #[cfg(not(loom))]
    pub use std::sync::Arc;
    #[cfg(not(loom))]
    pub use std::sync::atomic::{AtomicUsize, Ordering};
}

#[cfg(not(loom))]
trait UnsafeCellExt<T> {
    fn with_mut<F, R>(&self, f: F) -> R
    where
        F: FnOnce(*mut T) -> R;
}
#[cfg(not(loom))]
impl<T> UnsafeCellExt<T> for std::cell::UnsafeCell<T> {
    #[inline(always)]
    fn with_mut<F, R>(&self, f: F) -> R
    where
        F: FnOnce(*mut T) -> R,
    {
        f(self.get())
    }
}

use std::mem::MaybeUninit;
use sync::{Arc, AtomicUsize, Ordering, UnsafeCell};

/// Pad to a cache line so the producer's `tail` and consumer's `head` never
/// share one — otherwise each side's store would invalidate the other's cache
/// line (false sharing) and erase the point of a lock-free ring. 128 bytes
/// covers Apple-silicon's 128-byte prefetch pairs as well as x86's 64-byte line.
#[repr(align(128))]
struct CachePadded<T>(T);

struct Ring<T> {
    /// `capacity` slots; only indices in `[head, tail)` (mod capacity) are init.
    buf: Box<[UnsafeCell<MaybeUninit<T>>]>,
    /// `capacity - 1`; `capacity` is a power of two so `idx & mask` wraps.
    mask: usize,
    /// Next index to pop. Owned by the consumer; read by the producer.
    head: CachePadded<AtomicUsize>,
    /// Next index to push. Owned by the producer; read by the consumer.
    tail: CachePadded<AtomicUsize>,
}

// SAFETY: the SPSC contract (enforced by `&mut self` on push/pop and the split
// Producer/Consumer halves) means the producer only ever writes the slot at
// `tail` and advances `tail`, while the consumer only reads the slot at `head`
// and advances `head`. Those index ranges are disjoint, so the `UnsafeCell`
// accesses never alias. A `T: Send` may thus cross the producer→consumer thread
// boundary, making the shared `Ring` safe to `Send` and `Sync`.
unsafe impl<T: Send> Send for Ring<T> {}
unsafe impl<T: Send> Sync for Ring<T> {}

impl<T> Ring<T> {
    fn with_capacity(cap: usize) -> Self {
        // At least 2 slots; round up to a power of two for masking.
        let cap = cap.max(2).next_power_of_two();
        let mut v = Vec::with_capacity(cap);
        for _ in 0..cap {
            v.push(UnsafeCell::new(MaybeUninit::uninit()));
        }
        Ring {
            buf: v.into_boxed_slice(),
            mask: cap - 1,
            head: CachePadded(AtomicUsize::new(0)),
            tail: CachePadded(AtomicUsize::new(0)),
        }
    }
}

impl<T> Drop for Ring<T> {
    fn drop(&mut self) {
        // Drop the elements still queued (indices `[head, tail)`); the rest are
        // uninitialized and must not be touched.
        let head = self.head.0.load(Ordering::Relaxed);
        let tail = self.tail.0.load(Ordering::Relaxed);
        let mut i = head;
        while i != tail {
            // SAFETY: `i` is in `[head, tail)`, so this slot holds an initialized
            // `T` that no one else will read (we have `&mut self`).
            self.buf[i & self.mask].with_mut(|p| unsafe {
                (*p).assume_init_drop();
            });
            i = i.wrapping_add(1);
        }
    }
}

/// The sending half. `Send` (move to the producer thread); only this half pushes.
pub struct Producer<T> {
    inner: Arc<Ring<T>>,
    /// Cached snapshot of the consumer's `head`. Stale-OK: a value the
    /// consumer has already advanced past is still safe to treat as "head"
    /// (the available-slot count we compute is then a conservative lower
    /// bound, never letting us overwrite a live slot). Refreshed from the
    /// shared `head` only when the cached count says the ring is full.
    /// This is the SPSC fast-path lever — it amortises the cross-cache-line
    /// `Acquire` load on the consumer's cursor over many pushes.
    head_cache: usize,
}

/// The receiving half. `Send` (move to the consumer thread); only this half pops.
pub struct Consumer<T> {
    inner: Arc<Ring<T>>,
    /// Cached snapshot of the producer's `tail`. Stale-OK in the same way as
    /// [`Producer::head_cache`]: a value below the truth still lets us pop
    /// safely (we just see fewer items than really exist; the next refresh
    /// catches up). Refreshed only when the cached count says the ring is
    /// empty.
    tail_cache: usize,
}

/// Create a ring holding at least `capacity` items (rounded up to a power of
/// two, minimum 2), returning its producer and consumer halves.
pub fn ring<T>(capacity: usize) -> (Producer<T>, Consumer<T>) {
    let r = Arc::new(Ring::with_capacity(capacity));
    (
        Producer {
            inner: r.clone(),
            head_cache: 0,
        },
        Consumer {
            inner: r,
            tail_cache: 0,
        },
    )
}

impl<T> Producer<T> {
    /// Push `val`. Returns `Err(val)` (handing the value back) if the ring is
    /// full, so the caller can retry after the consumer drains.
    pub fn push(&mut self, val: T) -> Result<(), T> {
        let r = &*self.inner;
        // `tail` is ours: a plain Relaxed load suffices.
        let tail = r.tail.0.load(Ordering::Relaxed);
        // Fast path: trust the cached consumer head. If the cache says we
        // have room, skip the shared `Acquire` load entirely.
        if tail.wrapping_sub(self.head_cache) > r.mask {
            // Cache says full — refresh from the shared cursor. `Acquire`
            // pairs with the consumer's `Release` store of `head` so we
            // observe slots it has freed before we reuse them.
            self.head_cache = r.head.0.load(Ordering::Acquire);
            if tail.wrapping_sub(self.head_cache) > r.mask {
                return Err(val); // really full
            }
        }
        // SAFETY: slot `tail & mask` is free (outside `[head, tail)`); we are
        // the only producer, so no one else writes it.
        r.buf[tail & r.mask].with_mut(|p| unsafe {
            (*p).write(val);
        });
        // `Release` publishes the slot write to the consumer's `Acquire` of `tail`.
        r.tail.0.store(tail.wrapping_add(1), Ordering::Release);
        Ok(())
    }

    /// Whether the next [`push`](Self::push) would fail (ring full). Advisory:
    /// the consumer may free a slot immediately after this returns.
    pub fn is_full(&self) -> bool {
        let r = &*self.inner;
        let tail = r.tail.0.load(Ordering::Relaxed);
        let head = r.head.0.load(Ordering::Acquire);
        tail.wrapping_sub(head) > r.mask
    }

    /// Total slot count (a power of two ≥ 2).
    pub fn capacity(&self) -> usize {
        self.inner.mask + 1
    }
}

impl<T> Consumer<T> {
    /// Pop the oldest item, or `None` if the ring is empty.
    pub fn pop(&mut self) -> Option<T> {
        let r = &*self.inner;
        // `head` is ours: Relaxed.
        let head = r.head.0.load(Ordering::Relaxed);
        // Fast path: trust the cached producer tail. If the cache says we
        // have items, skip the shared `Acquire` load entirely.
        if head == self.tail_cache {
            // Cache says empty — refresh from the shared cursor. `Acquire`
            // pairs with the producer's `Release` store of `tail` so the
            // slot write is visible before we read it.
            self.tail_cache = r.tail.0.load(Ordering::Acquire);
            if head == self.tail_cache {
                return None; // really empty
            }
        }
        // SAFETY: slot `head & mask` is in `[head, tail)`, initialized by the
        // producer; we are the only consumer, so we read it exactly once.
        let val = r.buf[head & r.mask].with_mut(|p| unsafe { (*p).assume_init_read() });
        // `Release` frees the slot for the producer's `Acquire` of `head`.
        r.head.0.store(head.wrapping_add(1), Ordering::Release);
        Some(val)
    }

    /// Whether the next [`pop`](Self::pop) would return `None` (ring empty).
    /// Advisory: the producer may push immediately after this returns.
    pub fn is_empty(&self) -> bool {
        let r = &*self.inner;
        let head = r.head.0.load(Ordering::Relaxed);
        let tail = r.tail.0.load(Ordering::Acquire);
        head == tail
    }

    /// Number of items currently queued. Advisory under concurrent producing.
    pub fn len(&self) -> usize {
        let r = &*self.inner;
        let tail = r.tail.0.load(Ordering::Acquire);
        let head = r.head.0.load(Ordering::Relaxed);
        tail.wrapping_sub(head)
    }

    /// Total slot count (a power of two ≥ 2).
    pub fn capacity(&self) -> usize {
        self.inner.mask + 1
    }
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn drops_queued_elements_exactly_once() {
        // A payload that bumps a shared counter on drop; verify the ring's Drop
        // releases exactly the still-queued items (no leak, no double free).
        use std::sync::Arc as StdArc;
        let dropped = StdArc::new(AtomicUsize::new(0));
        struct Bomb(StdArc<AtomicUsize>);
        impl Drop for Bomb {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
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
        const N: u64 = 1_000_000;
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
        const N: u64 = 200_000;
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
            match rx.pop() {
                Some(v) => {
                    assert_eq!(v, next);
                    next += 1;
                    spins += 1;
                    if spins.is_multiple_of(1000) {
                        std::thread::yield_now(); // let the ring fill up
                    }
                }
                None => {
                    if done.load(Ordering::Acquire) && rx.is_empty() {
                        break;
                    }
                    std::thread::yield_now();
                }
            }
        }
        producer.join().unwrap();
        assert_eq!(next, N);
    }
}
