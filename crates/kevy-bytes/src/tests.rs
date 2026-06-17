//! Unit tests for [`SmallBytes`] (split out of `lib.rs` for file-size hygiene).
//!
//! Reaches into a few `pub(crate)` items (the `Heap` rep, `INLINE_CAP`,
//! `INLINE_LEN_MAX`) so the alloc-counter test and the forged-heap
//! reproducer can run inside the crate. They are not exposed to
//! downstream users.

use super::*;
use kevy_hash::KevyHash as _;
use std::hash::{Hash, Hasher};

#[test]
fn size_and_align() {
    assert_eq!(mem::size_of::<SmallBytes>(), 24);
    assert_eq!(mem::align_of::<SmallBytes>(), mem::align_of::<usize>());
}

#[test]
fn empty_is_inline() {
    let s = SmallBytes::new();
    assert!(s.is_inline());
    assert_eq!(s.len(), 0);
    assert!(s.is_empty());
    assert_eq!(s.as_slice(), b"");
}

#[test]
fn inline_one_byte() {
    let s = SmallBytes::from_slice(b"x");
    assert!(s.is_inline());
    assert_eq!(s.len(), 1);
    assert_eq!(s.as_slice(), b"x");
}

#[test]
fn inline_at_boundary_22() {
    let v: Vec<u8> = (0u8..22).collect();
    let s = SmallBytes::from_slice(&v);
    assert!(s.is_inline());
    assert_eq!(s.len(), 22);
    assert_eq!(s.as_slice(), v);
}

#[test]
fn heap_at_boundary_23() {
    let v: Vec<u8> = (0u8..23).collect();
    let s = SmallBytes::from_slice(&v);
    assert!(!s.is_inline());
    assert_eq!(s.len(), 23);
    assert_eq!(s.as_slice(), v);
}

#[test]
fn heap_large() {
    let v: Vec<u8> = (0..4096).map(|i| (i & 0xFF) as u8).collect();
    let s = SmallBytes::from_slice(&v);
    assert!(!s.is_inline());
    assert_eq!(s.len(), 4096);
    assert_eq!(s.as_slice(), v.as_slice());
}

#[test]
fn from_vec_inline() {
    let s = SmallBytes::from_vec(vec![1u8, 2, 3]);
    assert!(s.is_inline());
    assert_eq!(s.as_slice(), &[1, 2, 3]);
}

#[test]
fn from_vec_heap_reuses_alloc() {
    let mut v: Vec<u8> = (0u8..100).collect();
    v.reserve(200);
    let ptr_before = v.as_ptr();
    let cap_before = v.capacity();
    let s = SmallBytes::from_vec(v);
    assert!(!s.is_inline());
    // SAFETY: we know it's heap; peek to verify pointer reuse.
    unsafe {
        assert_eq!(s.heap.ptr.as_ptr().cast_const(), ptr_before);
        assert_eq!(s.heap.capacity(), cap_before);
    }
}

#[test]
fn into_vec_inline_copies() {
    let s = SmallBytes::from_slice(b"hello");
    let v = s.into_vec();
    assert_eq!(v, b"hello");
}

#[test]
fn into_vec_heap_reuses_alloc() {
    let original: Vec<u8> = (0u8..200).collect();
    let ptr = original.as_ptr();
    let cap = original.capacity();
    let s = SmallBytes::from_vec(original);
    let v = s.into_vec();
    assert_eq!(v.as_ptr(), ptr);
    assert_eq!(v.capacity(), cap);
    assert_eq!(v.len(), 200);
}

#[test]
fn clone_inline() {
    let s = SmallBytes::from_slice(b"abc");
    let c = s.clone();
    assert_eq!(s, c);
    assert!(c.is_inline());
}

#[test]
fn clone_heap() {
    let v: Vec<u8> = (0u8..50).collect();
    let s = SmallBytes::from_slice(&v);
    let c = s.clone();
    assert_eq!(s, c);
    assert!(!c.is_inline());
}

#[test]
fn eq_by_content() {
    let a = SmallBytes::from_slice(b"short");
    let b = SmallBytes::from_slice(b"short");
    assert_eq!(a, b);
    let c: Vec<u8> = (0u8..30).collect();
    let d: Vec<u8> = (0u8..30).collect();
    assert_eq!(SmallBytes::from_slice(&c), SmallBytes::from_slice(&d));
}

#[test]
fn ord_lex() {
    let a = SmallBytes::from_slice(b"abc");
    let b = SmallBytes::from_slice(b"abd");
    assert!(a < b);
}

#[test]
fn debug_format_matches_slice() {
    let s = SmallBytes::from_slice(&[1u8, 2, 3]);
    let dbg = format!("{s:?}");
    let exp = format!("{:?}", &[1u8, 2, 3][..]);
    assert_eq!(dbg, exp);
}

#[test]
fn default_is_empty_inline() {
    let s = SmallBytes::default();
    assert!(s.is_inline());
    assert_eq!(s.len(), 0);
}

#[test]
fn drop_heap_does_not_leak_or_double_free() {
    // Loop a bunch to give miri/asan something to catch.
    for n in [23usize, 64, 1024, 65536] {
        let v: Vec<u8> = (0..n).map(|i| (i & 0xFF) as u8).collect();
        let s = SmallBytes::from_slice(&v);
        drop(s);
    }
}

// ---- Effective coverage: trait impls + branch paths ---------------------

#[test]
fn eq_is_reflexive_and_symmetric_inline() {
    let a = SmallBytes::from_slice(b"hi");
    let b = SmallBytes::from_slice(b"hi");
    let c = SmallBytes::from_slice(b"no");
    assert_eq!(a, a);
    assert_eq!(a, b);
    assert_eq!(b, a);
    assert_ne!(a, c);
}

#[test]
fn eq_is_reflexive_and_symmetric_heap() {
    let v: Vec<u8> = (0u8..40).collect();
    let a = SmallBytes::from_slice(&v);
    let b = SmallBytes::from_slice(&v);
    let mut w = v.clone();
    w[0] = w[0].wrapping_add(1);
    let c = SmallBytes::from_slice(&w);
    assert_eq!(a, a);
    assert_eq!(a, b);
    assert_eq!(b, a);
    assert_ne!(a, c);
}

#[test]
fn partial_cmp_matches_cmp_inline() {
    let a = SmallBytes::from_slice(b"abc");
    let b = SmallBytes::from_slice(b"abd");
    assert_eq!(a.partial_cmp(&b), Some(std::cmp::Ordering::Less));
    assert_eq!(b.partial_cmp(&a), Some(std::cmp::Ordering::Greater));
    assert_eq!(a.partial_cmp(&a), Some(std::cmp::Ordering::Equal));
    // Same chain via the Ord impl directly.
    assert_eq!(a.cmp(&b), std::cmp::Ordering::Less);
    assert_eq!(a.cmp(&a), std::cmp::Ordering::Equal);
}

#[test]
fn hash_agrees_with_byte_slice() {
    use std::collections::hash_map::DefaultHasher;
    let v: Vec<u8> = (0u8..40).collect();
    let s = SmallBytes::from_slice(&v);
    let mut h_slice = DefaultHasher::new();
    v.as_slice().hash(&mut h_slice);
    let mut h_sb = DefaultHasher::new();
    s.hash(&mut h_sb);
    // Same byte stream into the Hasher (Hash for [u8] writes len + bytes;
    // ours delegates to as_slice so it matches).
    assert_eq!(h_slice.finish(), h_sb.finish());
}

#[test]
fn kevy_hash_agrees_with_byte_slice() {
    let v: Vec<u8> = (0u8..40).collect();
    let s = SmallBytes::from_slice(&v);
    assert_eq!(
        s.kevy_hash(),
        v.as_slice().kevy_hash(),
        "KevyHash impl must agree with &[u8] so a KevyMap<SmallBytes, V> can be queried by Borrow<[u8]>"
    );
    let small = SmallBytes::from_slice(b"foo");
    assert_eq!(small.kevy_hash(), (b"foo" as &[u8]).kevy_hash());
}

#[test]
fn as_ref_is_zero_copy_view() {
    let s = SmallBytes::from_slice(b"abcdef");
    let r: &[u8] = s.as_ref();
    assert_eq!(r, b"abcdef");
    // Same slice address as as_slice (the impl delegates to as_slice).
    assert!(std::ptr::eq(r.as_ptr(), s.as_slice().as_ptr()));
}

#[test]
fn borrow_lookup_works_in_collection() {
    use std::collections::HashMap;
    let mut m: HashMap<SmallBytes, i32> = HashMap::new();
    m.insert(SmallBytes::from_slice(b"key1"), 1);
    m.insert(SmallBytes::from_slice(b"key2"), 2);
    // Look up by &[u8] thanks to Borrow<[u8]>.
    assert_eq!(m.get(b"key1".as_slice()), Some(&1));
    assert_eq!(m.get(b"key2".as_slice()), Some(&2));
    assert_eq!(m.get(b"none".as_slice()), None);
}

#[test]
fn from_byte_slice_round_trip() {
    let a: SmallBytes = (&b"short"[..]).into();
    assert_eq!(a.as_slice(), b"short");
    let v: Vec<u8> = (0u8..40).collect();
    let b: SmallBytes = v.as_slice().into();
    assert_eq!(b.as_slice(), v.as_slice());
    assert!(!b.is_inline());
}

#[test]
fn from_vec_dispatches_inline_or_heap() {
    // ≤ 22 → inline (copies)
    let inline_src: SmallBytes = vec![1u8, 2, 3].into();
    assert!(inline_src.is_inline());
    assert_eq!(inline_src.as_slice(), &[1, 2, 3]);
    // > 22 → heap (reuses alloc; verified by from_vec_heap_reuses_alloc)
    let v: Vec<u8> = (0u8..30).collect();
    let heap_src: SmallBytes = v.clone().into();
    assert!(!heap_src.is_inline());
    assert_eq!(heap_src.as_slice(), v.as_slice());
}

#[test]
fn clone_heap_keeps_data_and_is_independent() {
    // Cloned heap value must allocate a separate buffer (no shared
    // pointer), so dropping the source doesn't invalidate the clone.
    let v: Vec<u8> = (0u8..50).collect();
    let src = SmallBytes::from_slice(&v);
    let dup = src.clone();
    // SAFETY: both in heap variant by len > 22.
    unsafe {
        assert_ne!(
            src.heap.ptr.as_ptr(),
            dup.heap.ptr.as_ptr(),
            "clone must allocate a fresh buffer"
        );
    }
    drop(src);
    // dup remains valid.
    assert_eq!(dup.as_slice(), v.as_slice());
}

#[test]
fn drop_inline_is_noop() {
    // Just exercise the inline path of Drop (the `if self.is_inline()
    // { return }` early-return); miri checks no UB.
    for &n in &[0usize, 1, 5, 22] {
        let s = SmallBytes::from_slice(&vec![b'x'; n]);
        assert!(s.is_inline());
        drop(s);
    }
}

#[test]
fn into_vec_zero_size_path() {
    // Empty (inline) → into_vec returns empty Vec without panic.
    let s = SmallBytes::new();
    let v = s.into_vec();
    assert!(v.is_empty());
}

#[test]
fn to_vec_copies_inline_and_heap() {
    let inline = SmallBytes::from_slice(b"hi");
    assert_eq!(inline.to_vec(), b"hi");
    let v: Vec<u8> = (0u8..30).collect();
    let heap = SmallBytes::from_slice(&v);
    let copy = heap.to_vec();
    assert_eq!(copy, v);
    // to_vec returns an owned independent Vec; heap can be modified
    // via subsequent operations without affecting the returned Vec.
    // (Just verify equality after going through .to_vec.)
    assert_eq!(heap.as_slice(), v.as_slice());
}

// ===== alloc-count test =====
//
// The whole point of SmallBytes' SSO is "no heap alloc when payload ≤ 22
// bytes". We can prove it by swapping in a counting allocator and asserting
// the inline path produces ZERO Allocator::alloc calls. A heap-bound payload
// produces at least one. Wrapping the system allocator (not replacing it
// wholesale with a fake) keeps the test compatible with Rust's std types
// that the tests themselves use.
//
// Concurrency: the global allocator is shared by EVERY thread in the test
// process, and `cargo test` runs ~30 unrelated tests in this crate in
// parallel. A simple global flag would attribute their allocs to our
// measurement window. We instead key the recording on a thread-local so
// only the test thread *currently inside* `measure_allocs` counts.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

struct CountingAlloc {
    inner: System,
}

thread_local! {
    // `const { Cell::new(...) }` is lazily-zero-init at thread spawn — no
    // heap alloc — so the allocator itself can safely consult them.
    static THREAD_RECORDING: Cell<bool> = const { Cell::new(false) };
    static THREAD_ALLOC_CALLS: Cell<usize> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // `try_with` so if the TLS is being destroyed (process teardown)
        // we still serve the alloc instead of panicking.
        let _ = THREAD_RECORDING.try_with(|r| {
            if r.get() {
                let _ = THREAD_ALLOC_CALLS.try_with(|c| c.set(c.get() + 1));
            }
        });
        // SAFETY: forwarding to the system allocator with the same layout.
        unsafe { self.inner.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarding to the system allocator with the same layout.
        unsafe { self.inner.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static COUNTING: CountingAlloc = CountingAlloc { inner: System };

fn measure_allocs<F: FnOnce()>(f: F) -> usize {
    THREAD_ALLOC_CALLS.with(|c| c.set(0));
    THREAD_RECORDING.with(|r| r.set(true));
    f();
    THREAD_RECORDING.with(|r| r.set(false));
    THREAD_ALLOC_CALLS.with(std::cell::Cell::get)
}

#[test]
fn inline_payload_does_not_allocate() {
    // Warm + capture: every inline-sized SmallBytes constructor + access
    // must produce zero heap allocations. `INLINE_LEN_MAX` is the max
    // payload length the inline variant can hold (one byte of the
    // INLINE_CAP-byte buffer is the length+discriminant tag).
    let max_inline = INLINE_LEN_MAX as usize;
    let allocs = measure_allocs(|| {
        for n in 0..=max_inline {
            let s = SmallBytes::from_slice(&[0u8; INLINE_CAP][..n]);
            std::hint::black_box(&s);
            std::hint::black_box(s.as_slice());
            std::hint::black_box(s.len());
            let c = s.clone(); // Clone of an inline value is also alloc-free.
            std::hint::black_box(&c);
            drop(c);
            drop(s);
        }
    });
    assert_eq!(
        allocs, 0,
        "expected SSO inline path to be alloc-free, got {allocs} allocs"
    );
}

#[test]
fn heap_payload_does_allocate() {
    // Control: payload just over the inline cap MUST allocate. If this
    // is 0 either SSO bumped its cap silently or the counter is broken —
    // either way the inline-zero assertion above is meaningless.
    let max_inline = INLINE_LEN_MAX as usize;
    let allocs = measure_allocs(|| {
        let s = SmallBytes::from_slice(&[7u8; INLINE_CAP + 8][..=max_inline]);
        std::hint::black_box(&s);
        drop(s);
    });
    assert!(
        allocs >= 1,
        "expected the heap path to allocate at least once, got {allocs}"
    );
}

/// REAL prod incident (mailrs 2026-06-03): two legitimately-constructed
/// `SmallBytes` values — one inline (≤22 B) and one heap (>22 B) — get
/// compared by HashMap on a hash-collision. They have different
/// lengths, so they land in different union arms. Pre-fix: the
/// `unreachable!()` on the mixed arm panicked. Post-fix: falls back
/// to slice-form equality, which short-circuits on length internally
/// and returns `false` whenever the lengths differ. This is THE real
/// fix — not a defensive hack. The next test forges the same shape
/// but is the conceptual root-cause test.
#[test]
fn partial_eq_mixed_arm_does_not_panic() {
    use std::mem::ManuallyDrop;

    let inline_hi = SmallBytes::from_slice(b"hi");
    let inline_no = SmallBytes::from_slice(b"no");

    // Forge a heap variant that claims to hold "hi" with len = 2 —
    // invariant-violating, but mechanically possible if the union
    // bytes were ever externally written. The backing Vec stays
    // alive via ManuallyDrop so the forged pointer is valid for
    // the read inside PartialEq.
    let mut storage = ManuallyDrop::new(b"hi".to_vec());
    let ptr = NonNull::new(storage.as_mut_ptr()).expect("non-null Vec");
    let forged = ManuallyDrop::new(SmallBytes {
        heap: Heap::new(ptr, 2, 2),
    });

    // Equal content: must return true, must NOT panic.
    assert_eq!(inline_hi, *forged);
    assert_eq!(*forged, inline_hi);
    // Different content: must return false, must NOT panic.
    assert_ne!(inline_no, *forged);
    assert_ne!(*forged, inline_no);

    // Drop sequence under miri's leak detector:
    //   - `forged` stays in ManuallyDrop forever — SmallBytes::Drop
    //     would dealloc(ptr, Layout(len=2, align=1)) using the
    //     forged cap, but the actual underlying allocation belongs
    //     to `storage` and likely has a different (larger) cap →
    //     wrong-layout dealloc = UB. We never run that Drop.
    //   - `storage` is the real owner; drop it explicitly so the
    //     Vec's allocation is released. After this point any
    //     access through forged.heap.ptr would be a use-after-
    //     free, but we don't touch it.
    let _ = forged;
    // SAFETY: storage hasn't been dropped yet and we won't access it
    // after this; the only outstanding alias (forged.heap.ptr) is
    // intentionally orphaned in `forged` which we never read again.
    unsafe { ManuallyDrop::drop(&mut storage); }
}

/// The actual mailrs prod crash shape, reproduced without unsafe:
/// a legitimately-inline short value compared against a
/// legitimately-heap long value. Different lengths, both correctly
/// constructed, but they take different union arms. Pre-fix this
/// panicked at `unreachable!()`; post-fix it just returns `false`.
///
/// Naturally produced by HashMap probing on hash-collision between
/// keys of different sizes — `_health_probe` (13 B inline) vs a
/// longer cement key (>22 B heap) in mailrs's case.
#[test]
fn partial_eq_unequal_length_across_inline_heap_is_false() {
    let short_inline = SmallBytes::from_slice(b"_health_probe"); // 13 B
    let long_heap = SmallBytes::from_slice(
        b"this string is definitely longer than twenty-two bytes",
    );
    // Sanity: pre-conditions of the shape.
    assert!(short_inline.is_inline());
    assert!(!long_heap.is_inline());
    // The real test: cross-arm comparison must NOT panic and must
    // return false because the lengths differ.
    assert_ne!(short_inline, long_heap);
    assert_ne!(long_heap, short_inline);
}
