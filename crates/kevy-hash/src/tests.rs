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

/// Frozen output. Not a property — the values themselves.
///
/// `kevy_hash()` on a key decides which `aof-{i}.aof` and `dump-{i}.rdb`
/// that key lives in (`kevy-embedded/src/shard.rs`), and `shards.meta`
/// records the routing SCHEME so a mismatch triggers a lossless
/// re-shard rather than stranding every key. But the scheme tag is
/// `"kevyhash"` for any version of this function: change a constant in
/// `hash_bytes_pipelined` and `prev == target` still holds, no re-shard
/// runs, and every key silently resolves to the wrong file.
///
/// Nothing caught that. Every other test here asks whether the output is
/// deterministic, differs from a neighbour, or spreads — all of which
/// survive any constant change. Perturbing `ANTI_ZERO` by one left all
/// 24 tests green. The CRC-16 side, checked the same way, killed four
/// mutations out of four, because it is pinned to a published value.
///
/// So this is the missing pin. If it fails, the hash moved: either put
/// it back, or bump the `shards.meta` routing tag to `"kevyhash-2"` in
/// the same change, so existing data directories take the migration path
/// instead of reading their keys out of the wrong files.
#[test]
fn the_hash_is_frozen_because_data_directories_depend_on_it() {
    // Bytes, by length: the boundaries of the 16-byte short path, the
    // bulk loop's first and last iteration, and either side of each.
    const BYTES: &[(usize, u64)] = &[
        (0, 0xa47e_4914_af8c_afbc),
        (1, 0xfd1f_9681_53ec_5dff),
        (3, 0x71fd_0807_d152_cf82),
        (4, 0x90fe_a4e1_40d5_fadc),
        (7, 0x65c4_056d_74f3_a28c),
        (8, 0x7dd8_25d7_01dd_6df2),
        (15, 0x80a6_e4b1_284a_aa04),
        (16, 0x1701_faea_6a41_d3e4),
        (17, 0xe866_a80f_5ce8_b61a),
        (32, 0x54e8_78d1_b9b0_f6d7),
        (33, 0x3bb0_5e16_2e71_4e14),
        (64, 0x85a5_2826_94af_6407),
        (200, 0x263a_c48d_68ef_b8b3),
    ];
    for &(n, want) in BYTES {
        let b: Vec<u8> = (0..n).map(|i| (i * 7 + 13) as u8).collect();
        assert_eq!(
            b.as_slice().kevy_hash(),
            want,
            "the byte hash moved at len {n} — see this test's note before changing it"
        );
    }

    // The integer impls route too (shard-by-id paths), and they take a
    // different code path from bytes, so a change to one need not show
    // up in the other.
    assert_eq!(0u64.kevy_hash(), 0x0000_0000_0000_0000);
    assert_eq!(1u64.kevy_hash(), 0x37e8_d294_6949_7cd2);
    assert_eq!(42u64.kevy_hash(), 0x2558_5839_4b61_ab76);
    assert_eq!(u64::MAX.kevy_hash(), 0x92f9_6f6a_0392_ef8d);

    // The floor: a table nobody walked would pass every assertion above.
    assert_eq!(BYTES.len(), 13, "the vector table shrank");
}
