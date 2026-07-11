//! Correctness workloads for the seqlock shared-read prototype: torn-read
//! detection under real write pressure, retry accounting, TTL-on-read
//! semantics, and the pin→clone→unpin Arc (zero-copy reply) protocol.

use crate::seqlock::{Ebr, ReadHit, RetireQueue, SeqEntry};
use kevy_map::KevyMap;
use kevy_store::{SmallBytes, Value};
use std::sync::Arc;
use std::time::Instant;

/// Payload classes rotate per write so a torn snapshot always crosses
/// shapes (different SmallBytes tag / pointer / length → detectable).
/// - class 0: 22 B  → `Value::Str` **inline** (SSO, no heap)
/// - class 1: 48 B  → `Value::Str` **heap** (spilled SmallBytes; < BULK_THRESHOLD)
/// - class 2: 128 B → `Value::ArcBulk` (> BULK_THRESHOLD)
/// - class 3: i64   → `Value::Int`
pub const CLASS_LENS: [usize; 3] = [22, 48, 128];

#[inline]
pub fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// Deterministic seed for (key, nonce). The payload is fully derivable
/// from its own first 8 bytes, so a reader can validate a value without
/// knowing which write produced it.
#[inline]
pub fn seed_of(key: u64, nonce: u64) -> u64 {
    splitmix64(key.wrapping_mul(0xC2B2_AE3D_27D4_EB4F) ^ nonce)
}

/// Build a payload: `[0..8]` = seed LE, `[8..16]` = expire_at_ns LE
/// (0 = no TTL), `[16..]` = pattern bytes derived from the seed.
pub fn build_payload(seed: u64, len: usize, expire_at_ns: u64) -> Vec<u8> {
    let mut v = vec![0u8; len];
    v[0..8].copy_from_slice(&seed.to_le_bytes());
    v[8..16].copy_from_slice(&expire_at_ns.to_le_bytes());
    for (j, b) in v.iter_mut().enumerate().skip(16) {
        *b = pattern_byte(seed, j);
    }
    v
}

#[inline]
fn pattern_byte(seed: u64, j: usize) -> u8 {
    ((seed >> ((j % 8) * 8)) as u8) ^ (j as u8)
}

/// Build the `Value` for (seed, class): the three byte classes route
/// through the exact kevy-store encodings (inline SmallBytes / spilled
/// SmallBytes / Arc<Box<[u8]>>), class 3 is `Value::Int`.
pub fn make_value(seed: u64, class: usize, expire_at_ns: u64) -> Value {
    match class {
        0 | 1 | 2 => {
            let payload = build_payload(seed, CLASS_LENS[class], expire_at_ns);
            if CLASS_LENS[class] > kevy_store::BULK_THRESHOLD {
                Value::ArcBulk(Arc::new(payload.into_boxed_slice()))
            } else {
                Value::Str(SmallBytes::from_vec(payload))
            }
        }
        _ => Value::Int(seed as i64),
    }
}

/// Validate a read-out payload. Returns `false` on any inconsistency —
/// i.e. a torn read the seqlock failed to catch.
pub fn validate_payload(bytes: &[u8], now_ns: u64, expired_hit: &mut u64) -> bool {
    if !CLASS_LENS.contains(&bytes.len()) {
        return false;
    }
    let seed = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
    let exp = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    // TTL-on-read semantics: a Hit whose own payload says it expired more
    // than 1 ms ago means the expire word and value words were torn apart.
    if exp != 0 && now_ns > exp + 1_000_000 {
        *expired_hit += 1;
    }
    bytes
        .iter()
        .enumerate()
        .skip(16)
        .all(|(j, &b)| b == pattern_byte(seed, j))
}

// ---------------------------------------------------------------------------
// Shared keyspace
// ---------------------------------------------------------------------------

pub struct SharedKeyspace {
    pub map: KevyMap<SmallBytes, SeqEntry>,
    pub ebr: Ebr,
    pub t0: Instant,
}

// SAFETY: the concurrent phase never mutates the map structurally (no
// insert/remove/rehash — pre-populated with enough capacity); all value
// mutation goes through SeqEntry's atomics. KevyMap probes are &self.
unsafe impl Sync for SharedKeyspace {}

pub fn key_name(i: usize) -> Vec<u8> {
    format!("key:{i:010}").into_bytes() // 14 B — inline SmallBytes key
}

impl SharedKeyspace {
    pub fn build(nkeys: usize, reader_slots: usize) -> Self {
        let mut map = KevyMap::with_capacity(nkeys * 2);
        for i in 0..nkeys {
            let seed = seed_of(i as u64, 0);
            map.insert(
                SmallBytes::from_vec(key_name(i)),
                SeqEntry::new(make_value(seed, i % 4, 0), 0),
            );
        }
        SharedKeyspace { map, ebr: Ebr::new(reader_slots), t0: Instant::now() }
    }

    #[inline]
    pub fn now_ns(&self) -> u64 {
        self.t0.elapsed().as_nanos() as u64
    }

    #[inline]
    pub fn entry(&self, key: &[u8]) -> &SeqEntry {
        self.map.get(key).expect("prototype keys are pre-populated")
    }
}

// ---------------------------------------------------------------------------
// Correctness runs
// ---------------------------------------------------------------------------

/// Retry histogram: buckets 0..=31 retries + overflow.
pub const HIST_BUCKETS: usize = 33;

pub struct ReadStats {
    pub reads: u64,
    pub hits: u64,
    pub expired: u64,
    pub torn: u64,
    /// Hits whose payload says the value expired ≥1 ms before the read —
    /// a torn expire/value pairing (must be 0).
    pub expired_hit: u64,
    pub arc_reads: u64,
    pub hist: [u64; HIST_BUCKETS],
}

impl Default for ReadStats {
    fn default() -> Self {
        ReadStats {
            reads: 0,
            hits: 0,
            expired: 0,
            torn: 0,
            expired_hit: 0,
            arc_reads: 0,
            hist: [0; HIST_BUCKETS],
        }
    }
}

impl ReadStats {
    pub fn merge(&mut self, o: &ReadStats) {
        self.reads += o.reads;
        self.hits += o.hits;
        self.expired += o.expired;
        self.torn += o.torn;
        self.expired_hit += o.expired_hit;
        self.arc_reads += o.arc_reads;
        for (a, b) in self.hist.iter_mut().zip(o.hist) {
            *a += b;
        }
    }

    pub fn percentile(&self, p: f64) -> usize {
        let total: u64 = self.hist.iter().sum();
        if total == 0 {
            return 0;
        }
        let want = (total as f64 * p).ceil() as u64;
        let mut acc = 0;
        for (i, n) in self.hist.iter().enumerate() {
            acc += n;
            if acc >= want {
                return i;
            }
        }
        HIST_BUCKETS - 1
    }

    pub fn max_retry(&self) -> usize {
        self.hist.iter().rposition(|&n| n > 0).unwrap_or(0)
    }
}

/// Reader loop: `n` reads over `keys` (uniform pseudo-random pick from
/// `key_lo..key_hi`), pin-per-read, every 4th read of an ArcBulk uses the
/// zero-copy Arc path and validates AFTER unpin (writev shape).
#[allow(clippy::too_many_arguments)]
pub fn reader_loop(
    ks: &SharedKeyspace,
    reader_id: usize,
    key_lo: usize,
    key_hi: usize,
    n: u64,
    stats: &mut ReadStats,
) {
    let mut rng = splitmix64(0xD1F0 + reader_id as u64);
    let mut out = Vec::with_capacity(256);
    let mut names: Vec<Vec<u8>> = (key_lo..key_hi).map(key_name).collect();
    // Pre-resolve nothing else: each op does the real map probe.
    let span = (key_hi - key_lo) as u64;
    let mut deferred: Vec<(Arc<Box<[u8]>>, u64)> = Vec::new();
    for i in 0..n {
        rng = splitmix64(rng);
        let key = &mut names[(rng % span) as usize];
        let arc_mode = i % 4 == 3;
        out.clear();
        let now = ks.now_ns();
        ks.ebr.pin(reader_id);
        let entry = ks.entry(key);
        let (hit, retries) = entry.read(now, &mut out, arc_mode);
        ks.ebr.unpin(reader_id);
        stats.reads += 1;
        stats.hist[(retries as usize).min(HIST_BUCKETS - 1)] += 1;
        match hit {
            ReadHit::Bytes => {
                stats.hits += 1;
                if !validate_payload(&out, now, &mut stats.expired_hit) {
                    stats.torn += 1;
                }
            }
            // Single-word payload — the seq validation covers the image;
            // consume the i64 so the lane isn't dead-coded away.
            ReadHit::Int(n) => {
                std::hint::black_box(n);
                stats.hits += 1;
            }
            ReadHit::Arc(a) => {
                stats.hits += 1;
                stats.arc_reads += 1;
                // Hold the Arc past the unpin, validate later (below) —
                // the writer may have overwritten + dropped its ref by
                // then; the refcount we took while pinned keeps it alive.
                deferred.push((a, now));
                if deferred.len() >= 64 {
                    for (a, t) in deferred.drain(..) {
                        if !validate_payload(&a, t, &mut stats.expired_hit) {
                            stats.torn += 1;
                        }
                    }
                }
            }
            ReadHit::Expired => stats.expired += 1,
        }
    }
    for (a, t) in deferred.drain(..) {
        if !validate_payload(&a, t, &mut stats.expired_hit) {
            stats.torn += 1;
        }
    }
}

pub struct WriteStats {
    pub writes: u64,
    pub retired: u64,
    pub freed: u64,
    pub max_parked: usize,
}

/// Owner-shard writer loop: `n` overwrites over its OWN key range
/// (shard-owned writes — no other thread writes these entries). Every
/// 64th write stamps a 300 µs TTL so readers exercise the expired-read
/// lane. Class rotates per nonce so consecutive writes always change
/// shape.
pub fn writer_loop(
    ks: &SharedKeyspace,
    key_lo: usize,
    key_hi: usize,
    n: u64,
    pace_spins: u32,
) -> WriteStats {
    let mut rq = RetireQueue::new(512);
    let names: Vec<Vec<u8>> = (key_lo..key_hi).map(key_name).collect();
    let span = (key_hi - key_lo) as u64;
    let mut rng = splitmix64(0xBEEF ^ key_lo as u64);
    for nonce in 1..=n {
        rng = splitmix64(rng);
        let ki = (rng % span) as usize;
        let key_id = (key_lo + ki) as u64;
        let seed = seed_of(key_id, nonce);
        let class = (nonce % 4) as usize;
        let exp = if nonce % 64 == 0 { ks.now_ns() + 300_000 } else { 0 };
        let old = ks.entry(&names[ki]).write(make_value(seed, class, exp), exp);
        rq.retire(&ks.ebr, old);
        for _ in 0..pace_spins {
            std::hint::spin_loop();
        }
    }
    rq.collect(&ks.ebr);
    let parked_left = rq.retired - rq.freed;
    rq.drain_all();
    WriteStats {
        writes: n,
        retired: rq.retired,
        freed: rq.freed - parked_left, // freed-through-EBR only
        max_parked: rq.max_parked,
    }
}

/// Config A — the 50/50 gate run: `w` owner-writers on disjoint key
/// ranges (shard-owned) + `r` readers reading UNIFORMLY over all keys
/// (the shared-read shape), equal op counts.
pub fn run_5050(nkeys: usize, w: usize, r: usize, ops: u64) -> (ReadStats, WriteStats, f64) {
    let ks = SharedKeyspace::build(nkeys, r);
    let mut merged = ReadStats::default();
    let mut wr = WriteStats { writes: 0, retired: 0, freed: 0, max_parked: 0 };
    let t = Instant::now();
    std::thread::scope(|s| {
        let mut rhs = Vec::new();
        for id in 0..r {
            let ks = &ks;
            rhs.push(s.spawn(move || {
                let mut st = ReadStats::default();
                reader_loop(ks, id, 0, nkeys, ops, &mut st);
                st
            }));
        }
        let mut whs = Vec::new();
        let per_w = nkeys / w;
        for wi in 0..w {
            let ks = &ks;
            whs.push(s.spawn(move || writer_loop(ks, wi * per_w, (wi + 1) * per_w, ops, 0)));
        }
        for h in rhs {
            merged.merge(&h.join().unwrap());
        }
        for h in whs {
            let ws = h.join().unwrap();
            wr.writes += ws.writes;
            wr.retired += ws.retired;
            wr.freed += ws.freed;
            wr.max_parked = wr.max_parked.max(ws.max_parked);
        }
    });
    (merged, wr, t.elapsed().as_secs_f64())
}

/// Config B — worst case: 1 owner-writer hammering ONE key, `r` readers
/// all reading that same key.
pub fn run_hotkey(r: usize, ops: u64) -> (ReadStats, u64) {
    let ks = SharedKeyspace::build(1, r);
    let mut merged = ReadStats::default();
    let mut writes = 0u64;
    std::thread::scope(|s| {
        let mut rhs = Vec::new();
        for id in 0..r {
            let ks = &ks;
            rhs.push(s.spawn(move || {
                let mut st = ReadStats::default();
                reader_loop(ks, id, 0, 1, ops, &mut st);
                st
            }));
        }
        let ks = &ks;
        let wh = s.spawn(move || writer_loop(ks, 0, 1, ops, 0));
        for h in rhs {
            merged.merge(&h.join().unwrap());
        }
        writes += wh.join().unwrap().writes;
    });
    (merged, writes)
}
