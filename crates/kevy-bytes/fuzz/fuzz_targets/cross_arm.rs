//! Fuzz cross-arm equality + HashMap collision shapes of `SmallBytes`.
//!
//! Motivated by the mailrs 2026-06-03 prod incident: `kevy-bytes 1.0.4`
//! crashed at PartialEq's mixed inline/heap arm via a normal hashbrown
//! hash-collision in a `HashMap<SmallBytes, _>`. The crate-level bench
//! exercises uniform-size keysets, which never produce a mixed-arm
//! comparison. This target naturally produces them by splitting the
//! libfuzzer corpus byte stream into two slices and exercising the
//! reachable trait operations across the inline/heap boundary.
//!
//! Properties asserted (by virtue of "doesn't panic / loop forever / OOM"):
//!   1. SmallBytes construction is total over `&[u8]`.
//!   2. PartialEq is total — including the mixed inline/heap arm that
//!      pre-1.1.1 had `unreachable!()` at.
//!   3. KevyHash is total.
//!   4. HashMap insertion + lookup is total across uniform-size and
//!      mixed-size populations.

#![no_main]

use kevy_bytes::SmallBytes;
use kevy_hash::FxHashMap;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    // Split byte-stream into two values. Using the first byte as the
    // pivot biases toward producing mixed inline/heap pairs (one side
    // ≤22 B → inline, other side >22 B → heap), which is exactly the
    // shape the mailrs incident reached.
    let pivot = (data[0] as usize) % (data.len() + 1);
    let (a_bytes, b_bytes) = data.split_at(pivot);

    let a = SmallBytes::from_slice(a_bytes);
    let b = SmallBytes::from_slice(b_bytes);

    // Same-arm and cross-arm `==`.
    let _ = a == b;
    let _ = b == a;
    let _ = a == a.clone();

    // Slice view + len agree.
    assert_eq!(a.as_slice().len(), a.len());

    // HashMap insertion exercises Hash + Eq under real hashbrown
    // probing. Using kevy-hash's FxHashMap matches the production
    // configuration where the crash originated.
    let mut m: FxHashMap<SmallBytes, ()> = FxHashMap::default();
    m.insert(a.clone(), ());
    m.insert(b.clone(), ());
    let _ = m.get(&a);
    let _ = m.get(&b);
    let _ = m.contains_key(&a);
    let _ = m.contains_key(&b);
});
