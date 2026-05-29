//! Keyspace hasher baseline — std default (SipHash) vs FNV-1a, *same* hashbrown
//! table.
//!
//! std's `HashMap` is already a world-class Swiss table (hashbrown); only its
//! default hasher is `SipHash-1-3` (DoS-resistant, but ~per-byte-keyed and slow
//! for short keys). kevy's keyspace is single-threaded per shard and never faces
//! adversarial key collisions across a trust boundary the way a public web map
//! does, so the SipHash tax buys us little. This bench asks: how much does
//! swapping *only the hasher* to FNV-1a (the constants kevy already uses for
//! shard routing) save, with the table left untouched?
//!
//! The variants run back-to-back in one process, so the **ratio** is meaningful
//! even on a loaded host (absolute ns drifts, ratio holds).
//!
//! Run: `cargo run -p kevy-store --example bench_keyspace --release`

use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};

use kevy_bench::{bench, black_box, compare, report};
use kevy_store::Store;

/// FNV-1a 64-bit — identical constants to kevy-rt's shard router
/// (`reduce.rs`: offset basis `0xcbf29ce484222325`, prime `0x100000001b3`).
struct Fnv1a(u64);

impl Default for Fnv1a {
    fn default() -> Self {
        Fnv1a(0xcbf2_9ce4_8422_2325)
    }
}

impl Hasher for Fnv1a {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        let mut h = self.0;
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        self.0 = h;
    }
}

#[derive(Clone, Default)]
struct FnvBuildHasher;

impl BuildHasher for FnvBuildHasher {
    type Hasher = Fnv1a;
    fn build_hasher(&self) -> Fnv1a {
        Fnv1a::default()
    }
}

/// FxHash-style — the same word-at-a-time shape SipHash uses, but one
/// `rotate ^ word * SEED` per 8 bytes instead of SipHash's multi-round mix.
/// No DoS resistance (no random seed), which a single-trust-domain keyspace
/// does not need. This is the real Tier-1 candidate after FNV lost.
#[derive(Default)]
struct FxHasher(u64);

const FX_SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;
const FX_ROTATE: u32 = 5;

#[inline]
fn fx_mix(state: u64, word: u64) -> u64 {
    (state.rotate_left(FX_ROTATE) ^ word).wrapping_mul(FX_SEED)
}

/// Feed `bytes` into `state` word-at-a-time (the fast path both Fx variants share).
#[inline]
fn fx_write(mut state: u64, mut bytes: &[u8]) -> u64 {
    while bytes.len() >= 8 {
        let word = u64::from_le_bytes(bytes[..8].try_into().unwrap());
        state = fx_mix(state, word);
        bytes = &bytes[8..];
    }
    if bytes.len() >= 4 {
        let word = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as u64;
        state = fx_mix(state, word);
        bytes = &bytes[4..];
    }
    for &b in bytes {
        state = fx_mix(state, b as u64);
    }
    state
}

/// murmur3 `fmix64` avalanche — spreads every input bit across all 64 output
/// bits. ~6 ALU ops, once per hash. This is what raw Fx lacks.
#[inline]
fn fmix64(mut h: u64) -> u64 {
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^= h >> 33;
    h
}

impl Hasher for FxHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        self.0 = fx_write(self.0, bytes);
    }
}

#[derive(Clone, Default)]
struct FxBuildHasher;

impl BuildHasher for FxBuildHasher {
    type Hasher = FxHasher;
    fn build_hasher(&self) -> FxHasher {
        FxHasher::default()
    }
}

/// Fx write speed + an `fmix64` finalizer for avalanche. The Tier-1 candidate
/// that should be *both* fast and well-distributed (no clustering).
#[derive(Default)]
struct FxMixHasher(u64);

impl Hasher for FxMixHasher {
    fn finish(&self) -> u64 {
        fmix64(self.0)
    }
    fn write(&mut self, bytes: &[u8]) {
        self.0 = fx_write(self.0, bytes);
    }
}

#[derive(Clone, Default)]
struct FxMixBuildHasher;

impl BuildHasher for FxMixBuildHasher {
    type Hasher = FxMixHasher;
    fn build_hasher(&self) -> FxMixHasher {
        FxMixHasher::default()
    }
}

const N: usize = 100_000;
const SAMPLES: usize = 60;
const INNER: usize = 50_000;

/// `n` keys of exactly `len` bytes: a distinct `"<prefix><i>"` head, padded with
/// `'x'` to a fixed length so we can sweep key size cleanly.
fn make_keys(prefix: &str, n: usize, len: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| {
            let mut k = format!("{prefix}{i}").into_bytes();
            k.resize(len, b'x');
            k
        })
        .collect()
}

fn suite(label: &str, keys: &[Vec<u8>], absent: &[Vec<u8>]) {
    let n = keys.len();
    println!("== {label}: {} bytes/key, N={n} ==", keys[0].len());

    let sip = RandomState::new();
    let fnv = FnvBuildHasher;
    let fx = FxBuildHasher;
    let fxm = FxMixBuildHasher;

    // 1. Pure hasher cost (no table) — the cleanest hasher signal.
    let mut i = 0usize;
    let h_sip = bench(SAMPLES, INNER, || {
        let k = &keys[i % n];
        i += 1;
        black_box(sip.hash_one(k));
    });
    i = 0;
    let h_fnv = bench(SAMPLES, INNER, || {
        let k = &keys[i % n];
        i += 1;
        black_box(fnv.hash_one(k));
    });
    i = 0;
    let h_fx = bench(SAMPLES, INNER, || {
        let k = &keys[i % n];
        i += 1;
        black_box(fx.hash_one(k));
    });
    i = 0;
    let h_fxm = bench(SAMPLES, INNER, || {
        let k = &keys[i % n];
        i += 1;
        black_box(fxm.hash_one(k));
    });
    println!(" hash_one (pure hasher):");
    compare("SipHash", h_sip, "FNV-1a ", h_fnv);
    compare("SipHash", h_sip, "FxHash ", h_fx);
    compare("SipHash", h_sip, "Fx+mix ", h_fxm);

    // Populate all three maps identically (same keys/values, different hasher).
    let mut m_sip: HashMap<Vec<u8>, u64> = HashMap::with_capacity(n);
    let mut m_fnv: HashMap<Vec<u8>, u64, FnvBuildHasher> =
        HashMap::with_capacity_and_hasher(n, FnvBuildHasher);
    let mut m_fx: HashMap<Vec<u8>, u64, FxBuildHasher> =
        HashMap::with_capacity_and_hasher(n, FxBuildHasher);
    let mut m_fxm: HashMap<Vec<u8>, u64, FxMixBuildHasher> =
        HashMap::with_capacity_and_hasher(n, FxMixBuildHasher);
    for (idx, k) in keys.iter().enumerate() {
        m_sip.insert(k.clone(), idx as u64);
        m_fnv.insert(k.clone(), idx as u64);
        m_fx.insert(k.clone(), idx as u64);
        m_fxm.insert(k.clone(), idx as u64);
    }

    // 2. GET hit — the hottest Redis op.
    i = 0;
    let g_sip = bench(SAMPLES, INNER, || {
        let k = &keys[i % n];
        i += 1;
        black_box(m_sip.get(k));
    });
    i = 0;
    let g_fnv = bench(SAMPLES, INNER, || {
        let k = &keys[i % n];
        i += 1;
        black_box(m_fnv.get(k));
    });
    i = 0;
    let g_fx = bench(SAMPLES, INNER, || {
        let k = &keys[i % n];
        i += 1;
        black_box(m_fx.get(k));
    });
    i = 0;
    let g_fxm = bench(SAMPLES, INNER, || {
        let k = &keys[i % n];
        i += 1;
        black_box(m_fxm.get(k));
    });
    println!(" get_hit:");
    compare("SipHash", g_sip, "FNV-1a ", g_fnv);
    compare("SipHash", g_sip, "FxHash ", g_fx);
    compare("SipHash", g_sip, "Fx+mix ", g_fxm);

    // 3. GET miss — hash + probe with no match (negative lookups, e.g. SETNX).
    i = 0;
    let mm_sip = bench(SAMPLES, INNER, || {
        let k = &absent[i % n];
        i += 1;
        black_box(m_sip.get(k));
    });
    i = 0;
    let mm_fnv = bench(SAMPLES, INNER, || {
        let k = &absent[i % n];
        i += 1;
        black_box(m_fnv.get(k));
    });
    i = 0;
    let mm_fx = bench(SAMPLES, INNER, || {
        let k = &absent[i % n];
        i += 1;
        black_box(m_fx.get(k));
    });
    i = 0;
    let mm_fxm = bench(SAMPLES, INNER, || {
        let k = &absent[i % n];
        i += 1;
        black_box(m_fxm.get(k));
    });
    println!(" get_miss:");
    compare("SipHash", mm_sip, "FNV-1a ", mm_fnv);
    compare("SipHash", mm_sip, "FxHash ", mm_fx);
    compare("SipHash", mm_sip, "Fx+mix ", mm_fxm);
}

/// Production hot path: the real `Store` (keyspace now uses Fx+fmix64). Absolute
/// per-op cost, the post-adoption keyspace-hasher baseline.
fn bench_real_store() {
    let keys = make_keys("key:", N, 12);
    let absent = make_keys("absent:", N, 12);
    let mut s = Store::new();
    for k in &keys {
        s.set(k, b"value-payload-16".to_vec(), None, false, false);
    }
    println!("== real Store::get/set (Fx+fmix64 keyspace), N={N} ==");

    let mut i = 0usize;
    let g = bench(SAMPLES, INNER, || {
        let k = &keys[i % N];
        i += 1;
        black_box(s.get(k).ok());
    });
    report("Store::get hit", g);
    i = 0;
    let m = bench(SAMPLES, INNER, || {
        let k = &absent[i % N];
        i += 1;
        black_box(s.get(k).ok());
    });
    report("Store::get miss", m);
    i = 0;
    let st = bench(SAMPLES, INNER, || {
        let k = &keys[i % N];
        i += 1;
        s.set(k, b"value-payload-16".to_vec(), None, false, false);
    });
    report("Store::set overwrite", st);

    // INCR path (live_entry_mut): repopulate numeric, then increment in place.
    for k in &keys {
        s.set(k, b"0".to_vec(), None, false, false);
    }
    i = 0;
    let inc = bench(SAMPLES, INNER, || {
        let k = &keys[i % N];
        i += 1;
        black_box(s.incr_by(k, 1).ok());
    });
    report("Store::incr_by", inc);
    println!();
}

fn main() {
    println!(
        "keyspace hasher baseline — std HashMap (hashbrown table), SipHash vs candidates\n\
         note: ratios are the signal; absolute ns drift with host load.\n"
    );
    let short = make_keys("key:", N, 12);
    let short_absent = make_keys("absent:", N, 12);
    suite("short key", &short, &short_absent);

    let long = make_keys("session:user:", N, 40);
    let long_absent = make_keys("absent:longer:", N, 40);
    suite("long key", &long, &long_absent);

    bench_real_store();
}
