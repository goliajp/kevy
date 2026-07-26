//! The shim under a real workload: this test binary's own allocator is
//! `KevyAlloc`, so every `Vec`, `String`, `Box` and channel below —
//! including the ones the test harness itself uses — goes through it.
//!
//! That is deliberate. A unit test that calls `Heap::alloc` directly
//! exercises the paths it thought to exercise; installing the allocator
//! makes the standard library the caller, and it asks for shapes nobody
//! would think to write down.
//!
//! Two acceptance lines live here: the shim works at all, and M5 —
//! frees arriving on a thread that did not allocate, which kevy does
//! routinely because values travel across shards on the shared read
//! lane.

use std::sync::mpsc;
use std::thread;

#[global_allocator]
static ALLOC: kevy_alloc::KevyAlloc = kevy_alloc::KevyAlloc;

/// Skip cleanly where anonymous mapping is unavailable — with no
/// mapping there is no allocator, and the process would not have
/// reached `main` to say so.
fn mapping_available() -> bool {
    kevy_alloc::os::available()
}

#[test]
fn the_standard_library_runs_on_it() {
    if !mapping_available() {
        eprintln!("skipped: no anonymous mapping on this target");
        return;
    }
    // Growth, reallocation, drop — the three things a Vec does.
    let mut v: Vec<u64> = Vec::new();
    for i in 0..100_000u64 {
        v.push(i * 3);
    }
    assert_eq!(v.len(), 100_000);
    assert_eq!(v[99_999], 299_997);
    v.retain(|x| x % 7 == 0);
    assert!(!v.is_empty());

    // Strings of wildly different sizes, so several classes and the
    // direct-mapping path are all in play.
    let mut kept = Vec::new();
    for n in [1usize, 17, 400, 8192, 8193, 70_000] {
        let s = "x".repeat(n);
        assert_eq!(s.len(), n);
        kept.push(s);
    }
    assert_eq!(kept.iter().map(String::len).sum::<usize>(), 86_803);

    // Over-aligned: 64 bytes is stricter than any size class, so this
    // takes the shim's own path rather than the heap's.
    let boxed: Box<Aligned64> = Box::new(Aligned64([7u8; 64]));
    assert_eq!(boxed.0[63], 7);
    assert_eq!(core::ptr::from_ref(&*boxed) as usize % 64, 0);
}

#[repr(align(64))]
struct Aligned64([u8; 64]);

#[test]
fn over_aligned_blocks_survive_a_round_trip() {
    if !mapping_available() {
        return;
    }
    // Several at once, so a mis-recorded base pointer corrupts a
    // neighbour rather than going unnoticed.
    let mut held: Vec<Box<Aligned64>> = Vec::new();
    for i in 0..500usize {
        held.push(Box::new(Aligned64([i as u8; 64])));
    }
    for (i, b) in held.iter().enumerate() {
        assert_eq!(core::ptr::from_ref(&**b) as usize % 64, 0, "block {i} lost its alignment");
        assert_eq!(b.0[0], i as u8, "block {i} was corrupted");
        assert_eq!(b.0[63], i as u8, "block {i} was corrupted at its end");
    }
}

/// A payload big enough to span several size classes, carrying a
/// checksum so a slot handed out twice cannot pass unnoticed.
fn make(seed: u64) -> Vec<u64> {
    let len = 4 + (seed as usize % 900);
    let mut v = Vec::with_capacity(len);
    for i in 0..len as u64 {
        v.push(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(i));
    }
    v
}

fn check(seed: u64, v: &[u64]) {
    let len = 4 + (seed as usize % 900);
    assert_eq!(v.len(), len, "length changed under seed {seed}");
    for (i, got) in v.iter().enumerate() {
        let want = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(i as u64);
        assert_eq!(*got, want, "seed {seed} element {i} was corrupted");
    }
}

/// M5 — allocations made on one thread and dropped on another.
///
/// Every allocation here crosses a thread boundary before it is freed,
/// so every deallocation takes the foreign path: the owning segment's
/// push-only list, drained by its owner. torajs-mmalloc documents a
/// Treiber ABA hazard and accepts it on the grounds that its runtime is
/// single-threaded; this is the test that says we may not inherit that
/// reasoning.
#[test]
fn m5_allocations_freed_on_a_foreign_thread_survive_intact() {
    if !mapping_available() {
        return;
    }
    const PRODUCERS: u64 = 4;
    const EACH: u64 = 5_000;

    let (tx, rx) = mpsc::channel::<(u64, Vec<u64>)>();
    let producers: Vec<_> = (0..PRODUCERS)
        .map(|p| {
            let tx = tx.clone();
            thread::spawn(move || {
                for i in 0..EACH {
                    let seed = p * EACH + i + 1;
                    tx.send((seed, make(seed))).expect("consumer alive");
                }
            })
        })
        .collect();
    drop(tx);

    // The consumer verifies and then drops — the drop is the foreign
    // free, on a thread that allocated none of this.
    let mut seen = 0u64;
    for (seed, payload) in rx {
        check(seed, &payload);
        seen += 1;
    }
    for p in producers {
        p.join().expect("producer finished");
    }
    assert_eq!(seen, PRODUCERS * EACH, "payloads went missing in transit");
}

/// The reverse direction as well: memory allocated on worker threads and
/// freed there, while the main thread's heap is also busy. Segments are
/// abandoned when their thread exits, which must leak address space
/// rather than unmap memory somebody still holds.
#[test]
fn m5_threads_may_exit_while_their_memory_is_still_held() {
    if !mapping_available() {
        return;
    }
    let mut survivors: Vec<Vec<u64>> = Vec::new();
    for round in 0..8u64 {
        let handle = thread::spawn(move || {
            let mut out = Vec::new();
            for i in 0..2_000u64 {
                out.push(make(round * 10_000 + i + 1));
            }
            out
        });
        // The producing thread is gone by the time we look at these.
        let batch = handle.join().expect("worker finished");
        for (i, v) in batch.iter().enumerate() {
            check(round * 10_000 + i as u64 + 1, v);
        }
        survivors.extend(batch);
    }
    assert_eq!(survivors.len(), 16_000);
    // Read them all again after every producing thread has exited: if a
    // thread's segments had been unmapped at exit, this would fault.
    for (i, v) in survivors.iter().enumerate() {
        let round = i as u64 / 2_000;
        let seed = round * 10_000 + (i as u64 % 2_000) + 1;
        check(seed, v);
    }
}

#[test]
fn thread_stats_balance_under_the_standard_library() {
    if !mapping_available() {
        return;
    }
    let mut ballast: Vec<Vec<u8>> = (0..2_000).map(|i| vec![i as u8; 300]).collect();
    let st = kevy_alloc::thread_stats().expect("thread-local heap is reachable");
    assert!(st.balanced(), "identity broken under real workload: {st:?}");
    assert!(st.live > 0, "the harness itself should be holding memory");
    ballast.clear();
    ballast.shrink_to_fit();
    kevy_alloc::thread_reclaim();
    let after = kevy_alloc::thread_stats().expect("still reachable");
    assert!(after.balanced(), "identity broken after reclaim: {after:?}");
}
