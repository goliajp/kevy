//! kevy-hash — fast, well-distributed hashing for kevy's single-trust-domain
//! keyspace. Zero dependencies.
//!
//! std's `HashMap` is a hashbrown Swiss table (excellent — kevy keeps it) keyed
//! by `SipHash-1-3` (DoS-resistant, but a tax a single-threaded-per-shard
//! keyspace facing no adversarial cross-trust key collisions does not need).
//! This crate supplies the hasher that table should use instead: an FxHash-style
//! word-at-a-time absorb plus a murmur3 [`fmix64`] avalanche finalizer.
//!
//! Measured (`rfcs/2026-05-25-std-self-host-evaluation.md`, via
//! `kevy-store/examples/bench_keyspace.rs`): ~4× faster hashing, ~1.2–2.8×
//! faster GET-hit, ~1.1–1.7× faster GET-miss than SipHash, with no clustering.
//! The finalizer is **essential** — the bare Fx absorb (no `fmix64`) clusters
//! 30–50× on low-entropy sequential keys like `"key:0".."key:99999"`.
//!
//! **Not DoS-resistant.** There is no random seed, so an attacker who can choose
//! keys *and* observe timing could force collisions. kevy's keyspace lives
//! inside one trust domain per shard, so this is the right trade; do not reuse
//! this hasher for maps fed untrusted, adversarially-chosen keys across a trust
//! boundary.
//!
//! ```
//! use kevy_hash::FxHashMap;
//!
//! let mut m: FxHashMap<Vec<u8>, u64> = FxHashMap::default();
//! m.insert(b"key".to_vec(), 1);
//! assert_eq!(m.get(b"key".as_slice()), Some(&1));
//! ```
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};

/// FxHash mixing constant (rustc's `rustc-hash` seed).
const SEED: u64 = 0x517c_c1b7_2722_0a95;
const ROTATE: u32 = 5;

/// murmur3 `fmix64` avalanche — spreads every input bit across all 64 output
/// bits. ~6 ALU ops, applied once on [`Hasher::finish`]. This is what the bare
/// Fx absorb lacks, and why it clusters without it.
#[inline]
pub fn fmix64(mut h: u64) -> u64 {
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^= h >> 33;
    h
}

#[inline]
fn mix(state: u64, word: u64) -> u64 {
    (state.rotate_left(ROTATE) ^ word).wrapping_mul(SEED)
}

/// Fast, well-distributed [`Hasher`] for kevy's keyspace. Word-at-a-time absorb
/// (FxHash-style) finished with [`fmix64`]. See the crate docs for the security
/// trade-off.
#[derive(Default)]
pub struct FxHasher(u64);

impl Hasher for FxHasher {
    #[inline]
    fn finish(&self) -> u64 {
        fmix64(self.0)
    }

    #[inline]
    fn write(&mut self, mut bytes: &[u8]) {
        let mut state = self.0;
        while bytes.len() >= 8 {
            let word = u64::from_le_bytes(bytes[..8].try_into().unwrap());
            state = mix(state, word);
            bytes = &bytes[8..];
        }
        if bytes.len() >= 4 {
            let word = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as u64;
            state = mix(state, word);
            bytes = &bytes[4..];
        }
        for &b in bytes {
            state = mix(state, b as u64);
        }
        self.0 = state;
    }

    // Fixed-width integer keys (e.g. connection-id maps) skip the byte loop.
    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.0 = mix(self.0, i);
    }
    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.0 = mix(self.0, i as u64);
    }
}

/// [`BuildHasher`](std::hash::BuildHasher) for [`FxHasher`]. Seedless, so equal
/// keys hash equally across instances and process runs.
pub type FxBuildHasher = BuildHasherDefault<FxHasher>;

/// Single-call hashing for kevy's per-command hot path.
///
/// `std::hash::Hasher` is a state-machine API — every hash is `Hasher::default()`
/// → `write_*` → `finish`, with `BuildHasher` indirection on top. For
/// `kevy-map`'s open-addressing table the keyspace is a small handful of
/// well-known leaf types (`[u8]`, `u32`, `u64`, `i32`); we get a faster, inline-
/// friendly hash by exposing one method on each that produces the final mixed
/// 64-bit value in one go.
///
/// All impls must agree with feeding the value through [`FxHasher`] then
/// calling `finish` — this lets us cut the trait dispatch without changing the
/// hash function. `kevy-map` consumes both the full hash (for bucket index)
/// and its top 7 bits (for the metadata byte).
pub trait KevyHash {
    /// Compute the final mixed 64-bit hash of `self` in one call.
    fn kevy_hash(&self) -> u64;
}

impl KevyHash for [u8] {
    #[inline]
    fn kevy_hash(&self) -> u64 {
        let mut h = FxHasher::default();
        h.write(self);
        h.finish()
    }
}

impl KevyHash for Vec<u8> {
    #[inline]
    fn kevy_hash(&self) -> u64 {
        self.as_slice().kevy_hash()
    }
}

impl KevyHash for u64 {
    #[inline]
    fn kevy_hash(&self) -> u64 {
        fmix64(mix(0, *self))
    }
}

impl KevyHash for u32 {
    #[inline]
    fn kevy_hash(&self) -> u64 {
        fmix64(mix(0, *self as u64))
    }
}

impl KevyHash for i32 {
    #[inline]
    fn kevy_hash(&self) -> u64 {
        // Sign-extend to u64 so equal i32 values hash the same as if widened
        // through the integer path; negatives' top bits still fmix64 away.
        fmix64(mix(0, *self as i64 as u64))
    }
}

impl KevyHash for usize {
    #[inline]
    fn kevy_hash(&self) -> u64 {
        fmix64(mix(0, *self as u64))
    }
}

/// A [`HashMap`] using [`FxHasher`] instead of SipHash.
pub type FxHashMap<K, V> = HashMap<K, V, FxBuildHasher>;

/// A [`HashSet`] using [`FxHasher`] instead of SipHash.
pub type FxHashSet<T> = HashSet<T, FxBuildHasher>;

#[cfg(test)]
mod tests {
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
    fn kevy_hash_matches_fx_hasher_for_bytes() {
        // KevyHash is the one-call form of: FxHasher::default(); write(); finish().
        // (Note: BuildHasher::hash_one routes through <[u8] as Hash> which adds a
        // length prefix — we match the raw-Fx path, not hash_one. kevy-map is
        // standalone; the FxHashMap path stays available for callers that want
        // hash_one's length-prefixed behaviour.)
        let key = b"hello-world".as_slice();
        let one_call = key.kevy_hash();
        let mut staged = FxHasher::default();
        staged.write(key);
        assert_eq!(one_call, staged.finish());
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
}
