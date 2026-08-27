//! Unit tests for `kevy-hash`.
//!
//! Split out of `lib.rs` when that file reached the workspace's
//! 500-line ceiling — the same shape `kevy-uring/src/ring_tests.rs`
//! and `kevy-alloc/src/tests.rs` already take. Still a child module
//! of the crate root, so `use super::*` reaches everything private.

use super::*;
use std::hash::BuildHasher;

fn h(bytes: &[u8]) -> u64 {
    FxBuildHasher::default().hash_one(bytes)
}

#[test]
fn deterministic_across_instances() {
    assert_eq!(h(b"hello"), h(b"hello"));
    assert_ne!(h(b"hello"), h(b"hellp"));
    assert_ne!(h(b""), h(b"\0"));
}

#[test]
fn map_roundtrip() {
    let mut m: FxHashMap<Vec<u8>, u64> = FxHashMap::default();
    for i in 0..10_000u64 {
        m.insert(format!("key:{i}").into_bytes(), i);
    }
    assert_eq!(m.len(), 10_000);
    for i in 0..10_000u64 {
        assert_eq!(m.get(format!("key:{i}").into_bytes().as_slice()), Some(&i));
    }
}

#[test]
fn kevy_hash_bytes_is_deterministic_and_distinct() {
    // KevyHash for [u8] uses the two-stream pipelined hash_bytes_pipelined
    // path (the rustc-hash 2.x trick + our fmix64 finalize). It diverges
    // from the legacy FxHasher::write byte absorb path — see the impl
    // doc-comment.
    let key = b"hello-world".as_slice();
    // Deterministic across calls (no random seed).
    assert_eq!(key.kevy_hash(), key.kevy_hash());
    // Distinct from a single-bit-flipped key.
    assert_ne!(key.kevy_hash(), b"hello-worle".as_slice().kevy_hash());
    // Length matters (XOR-only short path otherwise wouldn't distinguish).
    assert_ne!(b"abc".as_slice().kevy_hash(), b"abcd".as_slice().kevy_hash());
    // The legacy FxHasher path is still available via std Hasher trait
    // (FxHashMap users); the two no longer have to agree.
    let mut staged = FxHasher::default();
    staged.write(key);
    let _fx_legacy = staged.finish();
    // Intentionally no assert_eq! here — divergence is the point.
}

#[test]
fn kevy_hash_integer_paths_differ_per_value() {
    let a: u64 = 1;
    let b: u64 = 2;
    assert_ne!(a.kevy_hash(), b.kevy_hash());
    let i: i32 = -1;
    let j: i32 = 1;
    assert_ne!(i.kevy_hash(), j.kevy_hash());
}

#[test]
fn kevy_hash_top7_bits_distribute() {
    // Same low-entropy clustering guard, but driven through `kevy_hash`
    // on byte slices — the path kevy-map's metadata byte will use.
    let mut top = [0u32; 128];
    for i in 0..4096u64 {
        let mut k = format!("key:{i}").into_bytes();
        k.resize(12, b'x');
        let hash = k.as_slice().kevy_hash();
        top[(hash >> 57) as usize] += 1;
    }
    let max = *top.iter().max().unwrap();
    assert!(max < 128, "top-7-bit skew {max} (mean 32) — avalanche failing");
}

#[test]
fn integer_keys_roundtrip() {
    let mut m: FxHashMap<u64, u64> = FxHashMap::default();
    for i in 0..1_000u64 {
        m.insert(i, i * 2);
    }
    assert_eq!(m.get(&500), Some(&1_000));
    assert_eq!(m.get(&999), Some(&1_998));
}

/// Guards against the raw-Fx failure mode: low-entropy sequential keys
/// (`"key:0xxxxx".."key:99999x"`) must spread across buckets, not pile up.
/// `fmix64` is what makes this pass; removing it would fail loudly.
#[test]
fn no_catastrophic_clustering_on_low_entropy_keys() {
    let keys: Vec<Vec<u8>> = (0..4096u64)
        .map(|i| {
            let mut k = format!("key:{i}").into_bytes();
            k.resize(12, b'x');
            k
        })
        .collect();

    // Low bits drive the bucket index; 4096 keys / 256 → mean 16/bucket.
    let mut low = [0u32; 256];
    // Top 7 bits drive hashbrown's SIMD control byte; / 128 → mean 32.
    let mut top = [0u32; 128];
    for k in &keys {
        let hash = h(k);
        low[(hash & 0xff) as usize] += 1;
        top[(hash >> 57) as usize] += 1;
    }
    let max_low = *low.iter().max().unwrap();
    let max_top = *top.iter().max().unwrap();
    // Well-avalanched ⇒ no bucket exceeds ~4× the mean.
    assert!(max_low < 64, "low-bit skew {max_low} (mean 16) — avalanche failing");
    assert!(max_top < 128, "top-bit skew {max_top} (mean 32) — avalanche failing");
}

// ---- KevyHash impls for delegating types (cov for u32 / usize / Vec<u8>) -

#[test]
fn kevy_hash_vec_u8_agrees_with_slice() {
    let v: Vec<u8> = b"hello-world".to_vec();
    assert_eq!(v.kevy_hash(), v.as_slice().kevy_hash());
}

#[test]
fn kevy_hash_u32_agrees_with_widened_u64() {
    // u32 widens through u64 → same hash as the u64 form of the same value.
    let n: u32 = 0xCAFE_BABE;
    assert_eq!(n.kevy_hash(), u64::from(n).kevy_hash());
    // Distinct values produce distinct hashes.
    let m: u32 = n.wrapping_add(1);
    assert_ne!(n.kevy_hash(), m.kevy_hash());
}

#[test]
fn kevy_hash_usize_agrees_with_u64() {
    // usize sign-free widens through u64. Equal-valued usize ↔ u64
    // must hash the same so a map keyed by either reads back equivalently.
    let n: usize = 42;
    assert_eq!(n.kevy_hash(), (n as u64).kevy_hash());
    let m: usize = 43;
    assert_ne!(n.kevy_hash(), m.kevy_hash());
}
