//! kevy-hash — fast, well-distributed hashing for kevy's single-trust-domain
//! keyspace. Zero dependencies.
//!
//! std's `HashMap` is a hashbrown Swiss table (excellent — kevy keeps it) keyed
//! by `SipHash-1-3` (DoS-resistant, but a tax a single-threaded-per-shard
//! keyspace facing no adversarial cross-trust key collisions does not need).
//! This crate supplies the hasher that table should use instead: an FxHash-style
//! word-at-a-time absorb plus a murmur3 [`fmix64`] avalanche finalizer.
//!
//! Measured (via `kevy-store/examples/bench_keyspace.rs`): ~4× faster
//! hashing, ~1.2–2.8× faster GET-hit, ~1.1–1.7× faster GET-miss than
//! SipHash, with no clustering.
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

/// Two-stream pipelined hash_bytes inspired by rustc-hash 2.x's design
/// (`rustc-hash/src/lib.rs#hash_bytes`). The key trick is keeping two
/// independent state words `s0` / `s1` updated via 64×64→128 widening
/// multiplication (one `mul`+`mulhi` on aarch64, one `mul` on x86_64),
/// XORing the two halves of the product to mix top with bottom. The two
/// streams are independent of each other in the bulk loop, so LLVM can
/// schedule them on two ALU ports per cycle.
///
/// Lengths ≤ 16: XOR-only absorb of two reads (start + end), then a
/// single `multiply_mix` of the two streams. The XOR-only absorb is fast
/// because there's no ALU dependency between the two reads.
///
/// Lengths > 16: per-16-byte iteration, `s1 <- multiply_mix(s0 ^ x,
/// CONST ^ y); s0 <- s1`. The `CONST` (digits of pi) prevents the
/// all-zeros input from collapsing.
///
/// Final mix: `multiply_mix(s0, s1) ^ len` — folds length in so that
/// `"abc"` and `"ab\0c"` hash differently (the XOR-only short path
/// doesn't distinguish length-by-position without this).
///
/// Then `fmix64` to give us the anti-clustering avalanche we need (the
/// rustc-hash design assumes its consumer mixes again; we don't, so we
/// avalanche ourselves — same property as the legacy [`FxHasher`] path).
#[inline]
fn hash_bytes_pipelined(bytes: &[u8]) -> u64 {
    // Constants — digits of pi (matches rustc-hash 2.x for cross-bench
    // sanity; the actual choice doesn't matter beyond "non-zero, not
    // sharing structure with input distributions").
    const S1: u64 = 0x243f_6a88_85a3_08d3;
    const S2: u64 = 0x1319_8a2e_0370_7344;
    const ANTI_ZERO: u64 = 0xa409_3822_299f_31d0;
    let len = bytes.len();
    let mut s0 = S1;
    let mut s1 = S2;

    if len <= 16 {
        if len >= 8 {
            // Read first 8 and last 8 (may overlap when 8 ≤ len ≤ 15).
            s0 ^= u64::from_le_bytes(bytes[0..8].try_into().unwrap());
            s1 ^= u64::from_le_bytes(bytes[len - 8..].try_into().unwrap());
        } else if len >= 4 {
            s0 ^= u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as u64;
            s1 ^= u32::from_le_bytes(bytes[len - 4..].try_into().unwrap()) as u64;
        } else if len > 0 {
            // 1-3 byte tail: form a 3-byte key (lo, mid, hi) that
            // distinguishes "ab" from "ba" etc.
            let lo = bytes[0];
            let mid = bytes[len / 2];
            let hi = bytes[len - 1];
            s0 ^= lo as u64;
            s1 ^= ((hi as u64) << 8) | mid as u64;
        }
        // len == 0 falls through with s0 == S1, s1 == S2 unchanged.
    } else {
        // Bulk: drop the very last byte from the bulk slice so the suffix
        // 16 bytes can partially overlap with bulk's tail (this is what
        // rustc-hash 2.x does; it makes the suffix path uniform).
        let mut bulk = &bytes[..len - 1];
        while let Some((chunk, rest)) = bulk.split_first_chunk::<16>() {
            let x = u64::from_le_bytes(chunk[..8].try_into().unwrap());
            let y = u64::from_le_bytes(chunk[8..].try_into().unwrap());
            let t = multiply_mix(s0 ^ x, ANTI_ZERO ^ y);
            s0 = s1;
            s1 = t;
            bulk = rest;
        }
        // Suffix 16 bytes (may overlap with last bulk iter).
        let suffix = &bytes[len - 16..];
        s0 ^= u64::from_le_bytes(suffix[0..8].try_into().unwrap());
        s1 ^= u64::from_le_bytes(suffix[8..16].try_into().unwrap());
    }

    let folded = multiply_mix(s0, s1) ^ (len as u64);
    fmix64(folded)
}

/// 64×64→128 widening multiply, XOR'ing the two halves of the product.
/// Single `mul` on x86_64, one `mul`+one `mulhi` on aarch64. Mixes top and
/// bottom of the product so the entire output fluctuates with small
/// changes in the input.
#[inline]
fn multiply_mix(x: u64, y: u64) -> u64 {
    let full = (x as u128).wrapping_mul(y as u128);
    let lo = full as u64;
    let hi = (full >> 64) as u64;
    lo ^ hi
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
    /// Byte-slice hash. Uses the **two-stream pipelined** path internally
    /// for ILP on the bench's 8-64 byte keyspace, closing the prior 1 ns
    /// gap vs rustc-hash 2.x's `hash_bytes`. The final `fmix64` retains
    /// the anti-clustering guarantee that the
    /// `no_catastrophic_clustering_on_low_entropy_keys` test enforces.
    ///
    /// Note: the result diverges from the legacy [`FxHasher`] absorb path —
    /// callers using `FxHashMap<Vec<u8>, _>` route through std's
    /// `Hash::hash → Hasher::write → finish` (the legacy single-stream
    /// path), which intentionally stays put for cross-instance hash
    /// stability with anything that depended on the v0.polish bit pattern.
    /// The `KevyHash for [u8]` impl is for one-call hot paths like
    /// `kevy-map::find_by_borrow`, which is the only one we measure.
    #[inline]
    fn kevy_hash(&self) -> u64 {
        hash_bytes_pipelined(self)
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
        assert_eq!(n.kevy_hash(), (n as u64).kevy_hash());
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
}
