//! Performance side of the seqlock shared-read gate:
//!  1. direct seqlock GET vs plain (owner-local) map GET — seqlock overhead;
//!  2. direct seqlock GET vs a kevy-ring forwarded round trip — the L1 prize;
//!  3. reader-scaling / cache-line contention matrix (per-entry version
//!     words vs one shared table version word).

use crate::workloads::{SharedKeyspace, build_payload, key_name, seed_of, splitmix64};
use kevy_map::KevyMap;
use kevy_store::{SmallBytes, Value};
use std::hint::black_box;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

/// Payload length for the perf lanes — mirrors the ≤30 B SmallReply class
/// of the main -d 3 axis (16 B keeps validation structure intact).
const PERF_LEN: usize = 16;
const NKEYS: usize = 1024;

/// Build a keyspace whose values are all 16 B inline `Value::Str` (the
/// perf shape: single cache line entry, no heap chase).
fn build_perf_keyspace(reader_slots: usize) -> SharedKeyspace {
    let ks = SharedKeyspace::build(NKEYS, reader_slots);
    for i in 0..NKEYS {
        let payload = build_payload(seed_of(i as u64, 1), PERF_LEN, 0);
        // The displaced setup value drops inline right here — safe because
        // no reader threads exist yet (single-threaded setup phase).
        drop(ks.entry(&key_name(i)).write(Value::Str(SmallBytes::from_vec(payload)), 0));
    }
    ks
}

// ---------------------------------------------------------------------------
// 1. direct seqlock read vs plain map read (single thread, uncontended)
// ---------------------------------------------------------------------------

pub struct DirectNumbers {
    pub plain_ns: f64,
    pub seq_pin_batch_ns: f64,
    pub seq_pin_op_ns: f64,
}

pub fn bench_direct(n: u64) -> DirectNumbers {
    // Plain owner-local map (today's S07' inline shape): KevyMap<_, Value>.
    let mut plain = KevyMap::with_capacity(NKEYS * 2);
    for i in 0..NKEYS {
        let payload = build_payload(seed_of(i as u64, 1), PERF_LEN, 0);
        plain.insert(SmallBytes::from_vec(key_name(i)), Value::Str(SmallBytes::from_vec(payload)));
    }
    let names: Vec<Vec<u8>> = (0..NKEYS).map(key_name).collect();
    let mut out = Vec::with_capacity(64);

    let mut rng = 7u64;
    let t = Instant::now();
    for _ in 0..n {
        rng = splitmix64(rng);
        let key = &names[(rng % NKEYS as u64) as usize];
        out.clear();
        if let Some(Value::Str(s)) = plain.get(key.as_slice()) {
            out.extend_from_slice(s.as_slice());
        }
        black_box(&out);
    }
    let plain_ns = t.elapsed().as_nanos() as f64 / n as f64;

    let ks = build_perf_keyspace(1);
    // Pin once per 16 ops (reactor-iteration pin amortisation).
    let mut rng = 7u64;
    let t = Instant::now();
    let mut i = 0u64;
    while i < n {
        ks.ebr.pin(0);
        for _ in 0..16 {
            rng = splitmix64(rng);
            let key = &names[(rng % NKEYS as u64) as usize];
            out.clear();
            let (h, _) = ks.entry(key).read(0, &mut out, false);
            black_box((&out, h));
            i += 1;
        }
        ks.ebr.unpin(0);
    }
    let seq_pin_batch_ns = t.elapsed().as_nanos() as f64 / i as f64;

    // Pin per op (worst-case pin accounting).
    let mut rng = 7u64;
    let t = Instant::now();
    for _ in 0..n {
        rng = splitmix64(rng);
        let key = &names[(rng % NKEYS as u64) as usize];
        out.clear();
        ks.ebr.pin(0);
        let (h, _) = ks.entry(key).read(0, &mut out, false);
        ks.ebr.unpin(0);
        black_box((&out, h));
    }
    let seq_pin_op_ns = t.elapsed().as_nanos() as f64 / n as f64;

    DirectNumbers { plain_ns, seq_pin_batch_ns, seq_pin_op_ns }
}

// ---------------------------------------------------------------------------
// 2. forwarded round trip (kevy-ring SPSC pair, busy-poll owner)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Req {
    idx: u32,
    seq: u32,
}

/// SmallReply-inline shape (≤30 B payload riding in the message).
#[derive(Clone, Copy)]
struct Resp {
    seq: u32,
    len: u8,
    bytes: [u8; 27],
}

/// S08 shape: forwarded request with the materialised verb+key argv
/// riding inline (the real chain copies ~25 B into a pooled Argv).
#[derive(Clone, Copy)]
struct FwdReq {
    seq: u32,
    klen: u8,
    argv: [u8; 30],
}

pub struct ForwardNumbers {
    /// Idealised: bare SPSC hop + reply copy, one ring item per op.
    pub hop_only_ns: f64,
    /// Faithful S08-S14 model at batch density 16 (single-key main axis):
    /// argv materialise → 16-op RequestBatch as one ring item + dirty
    /// fetch_or → owner dirty swap + batch exec + SmallReply copy →
    /// ResponseBatch → origin fold slots → in-order drain. Still EXCLUDES
    /// the reactor interleaving (arm loop, enter, slab recv) — a lower
    /// bound on the real chain.
    pub chain16_ns: f64,
    /// Same chain at batch density 2 — the spread (-r 1M) shape where
    /// per-target batch density collapses to ~16/7 and the amortisation
    /// dies (decomp §5.2: drain_inbound 2.17%→4.45%, send_to →1.85%).
    pub chain2_ns: f64,
}

const WINDOW: usize = 256;

pub fn bench_forward(n: u64) -> ForwardNumbers {
    ForwardNumbers {
        hop_only_ns: forward_hop_only(n),
        chain16_ns: forward_chain(n, 16),
        chain2_ns: forward_chain(n, 2),
    }
}

/// Idealised hop: origin pushes bare `Req`s, owner execs + pushes `Resp`s,
/// window 256 outstanding (throughput shape, like P16 across conns).
fn forward_hop_only(n: u64) -> f64 {
    let ks = build_perf_keyspace(1);
    let names: Vec<Vec<u8>> = (0..NKEYS).map(key_name).collect();
    let (mut req_tx, mut req_rx) = kevy_ring::ring::<Req>(1024);
    let (mut resp_tx, mut resp_rx) = kevy_ring::ring::<Resp>(1024);
    let stop = AtomicBool::new(false);

    let mut elapsed = 0.0f64;
    std::thread::scope(|s| {
        let ks = &ks;
        let names = &names;
        let stop = &stop;
        // Owner shard: busy-poll its inbound ring, exec, reply.
        s.spawn(move || {
            let mut out = Vec::with_capacity(64);
            loop {
                let mut worked = false;
                while let Some(req) = req_rx.pop() {
                    worked = true;
                    out.clear();
                    // Owner reads its own keyspace — no pin needed (the
                    // owner IS the writer; nothing frees under it).
                    let (_h, _) = ks.entry(&names[req.idx as usize]).read(0, &mut out, false);
                    let mut resp = Resp { seq: req.seq, len: out.len() as u8, bytes: [0; 27] };
                    resp.bytes[..out.len()].copy_from_slice(&out);
                    let mut r = resp;
                    while let Err(back) = resp_tx.push(r) {
                        r = back;
                        std::hint::spin_loop();
                    }
                }
                if !worked {
                    if stop.load(Ordering::Acquire) {
                        return;
                    }
                    std::hint::spin_loop();
                }
            }
        });

        // Origin shard: issue with a 256-op window, fold replies.
        let mut rng = 7u64;
        let mut sent = 0u64;
        let mut recvd = 0u64;
        let mut fold = Vec::with_capacity(64);
        let t = Instant::now();
        while recvd < n {
            while sent < n && sent - recvd < WINDOW as u64 {
                rng = splitmix64(rng);
                let req = Req { idx: (rng % NKEYS as u64) as u32, seq: sent as u32 };
                if req_tx.push(req).is_err() {
                    break;
                }
                sent += 1;
            }
            while let Some(resp) = resp_rx.pop() {
                fold.clear();
                fold.extend_from_slice(&resp.bytes[..resp.len as usize]);
                // seq rides along like the real fold's slot ordering key.
                black_box((resp.seq, &fold));
                recvd += 1;
            }
        }
        elapsed = t.elapsed().as_nanos() as f64;
        stop.store(true, Ordering::Release);
    });
    elapsed / n as f64
}

/// Faithful S08-S14 model (see decomp §3 stage table) at a given
/// per-target batch density.
fn forward_chain(n: u64, batch_size: usize) -> f64 {
    let ks = build_perf_keyspace(1);
    let names: Vec<Vec<u8>> = (0..NKEYS).map(key_name).collect();
    let (mut req_tx, mut req_rx) = kevy_ring::ring::<Vec<FwdReq>>(256);
    let (mut resp_tx, mut resp_rx) = kevy_ring::ring::<Vec<Resp>>(256);
    let dirty = AtomicU64::new(0);
    let stop = AtomicBool::new(false);

    let mut elapsed = 0.0f64;
    std::thread::scope(|s| {
        let ks = &ks;
        let names = &names;
        let stop = &stop;
        let dirty = &dirty;
        // Owner shard: S10 dirty swap + batch pop, S11 exec + SmallReply
        // copy, S12 ResponseBatch push.
        s.spawn(move || {
            let mut out = Vec::with_capacity(64);
            loop {
                let mut worked = false;
                if dirty.swap(0, Ordering::AcqRel) != 0 || !req_rx.is_empty() {
                    while let Some(rb) = req_rx.pop() {
                        worked = true;
                        let mut resps = Vec::with_capacity(rb.len());
                        for req in rb.iter() {
                            out.clear();
                            let (_h, _) =
                                ks.entry(&req.argv[..req.klen as usize]).read(0, &mut out, false);
                            let mut r = Resp { seq: req.seq, len: out.len() as u8, bytes: [0; 27] };
                            r.bytes[..out.len()].copy_from_slice(&out);
                            resps.push(r);
                        }
                        let mut b = resps;
                        while let Err(back) = resp_tx.push(b) {
                            b = back;
                            std::hint::spin_loop();
                        }
                    }
                }
                if !worked {
                    if stop.load(Ordering::Acquire) {
                        return;
                    }
                    std::hint::spin_loop();
                }
            }
        });

        // Origin shard: S08 argv materialise + batch accumulate, S09
        // batch flush + dirty fetch_or, S13 fold slots, S14 in-order drain.
        let mut rng = 7u64;
        let mut sent = 0u64;
        let mut next_emit = 0u64;
        let mut batch: Vec<FwdReq> = Vec::with_capacity(batch_size);
        let mut slots: Vec<Option<([u8; 27], u8)>> = vec![None; WINDOW];
        let mut fold = Vec::with_capacity(64);
        let t = Instant::now();
        while next_emit < n {
            // Issue up to the window.
            while sent < n && sent - next_emit < WINDOW as u64 {
                rng = splitmix64(rng);
                let key = &names[(rng % NKEYS as u64) as usize];
                let mut req = FwdReq { seq: sent as u32, klen: key.len() as u8, argv: [0; 30] };
                req.argv[..key.len()].copy_from_slice(key); // S08 memcpy
                batch.push(req);
                sent += 1;
                if batch.len() == batch_size {
                    flush_batch(&mut batch, &mut req_tx, dirty, batch_size);
                }
            }
            // Window full / input done: flush the partial batch so the
            // owner can make progress.
            if !batch.is_empty() {
                flush_batch(&mut batch, &mut req_tx, dirty, batch_size);
            }
            // S13: fold responses into seq slots.
            while let Some(rb) = resp_rx.pop() {
                for r in rb.iter() {
                    slots[r.seq as usize % WINDOW] = Some((r.bytes, r.len));
                }
            }
            // S14: drain in seq order into the conn output.
            while let Some((bytes, len)) = slots[(next_emit % WINDOW as u64) as usize].take() {
                fold.clear();
                fold.extend_from_slice(&bytes[..len as usize]);
                black_box(&fold);
                next_emit += 1;
            }
        }
        elapsed = t.elapsed().as_nanos() as f64;
        stop.store(true, Ordering::Release);
    });
    elapsed / n as f64
}

fn flush_batch(
    batch: &mut Vec<FwdReq>,
    tx: &mut kevy_ring::Producer<Vec<FwdReq>>,
    dirty: &AtomicU64,
    batch_size: usize,
) {
    let mut b = std::mem::replace(batch, Vec::with_capacity(batch_size));
    while let Err(back) = tx.push(b) {
        b = back;
        std::hint::spin_loop();
    }
    dirty.fetch_or(1, Ordering::Release); // S09 dirty publish
}

// ---------------------------------------------------------------------------
// 3. contention matrix
// ---------------------------------------------------------------------------

pub enum WriterMode {
    None,
    /// Full-speed overwrites of key 0 (hot-key worst case).
    HotKey,
    /// Paced overwrites over the lower half of the keyspace (disjoint from
    /// nothing — readers cover all keys; models real SET pressure).
    LowerHalf,
}

pub struct Cell {
    pub ns_per_read: f64,
    pub aggregate_mops: f64,
    pub retry_p99: usize,
}

/// Run `r` readers × `ops` reads each (hot single key or uniform over the
/// keyspace), optional writer, optional shared TABLE version word that
/// every read double-checks (cell F: the anti-design).
pub fn run_cell(r: usize, hot: bool, writer: WriterMode, table_word: bool, ops: u64) -> Cell {
    let ks = SharedKeyspace::build(NKEYS, r);
    let tbl = AtomicU64::new(0);
    let stop = AtomicBool::new(false);
    let hist = std::sync::Mutex::new([0u64; 33]);
    let mut wall = 0.0f64;

    std::thread::scope(|s| {
        let ks = &ks;
        let tbl = &tbl;
        let stop = &stop;
        let hist = &hist;
        let mut handles = Vec::new();
        for id in 0..r {
            handles.push(s.spawn(move || {
                let names: Vec<Vec<u8>> = (0..NKEYS).map(key_name).collect();
                let mut out = Vec::with_capacity(256);
                let mut rng = splitmix64(0xA11CE + id as u64);
                let mut h = [0u64; 33];
                let t = Instant::now();
                for _ in 0..ops {
                    rng = splitmix64(rng);
                    let ki = if hot { 0 } else { (rng % NKEYS as u64) as usize };
                    out.clear();
                    ks.ebr.pin(id);
                    let mut retries = 0u32;
                    loop {
                        let t1 = if table_word { tbl.load(Ordering::Acquire) } else { 0 };
                        out.clear();
                        let (hit, rr) = ks.entry(&names[ki]).read(0, &mut out, false);
                        retries += rr;
                        if table_word && tbl.load(Ordering::Acquire) != t1 {
                            retries += 1;
                            continue; // table version moved: retry (anti-design)
                        }
                        black_box((&out, hit));
                        break;
                    }
                    ks.ebr.unpin(id);
                    h[(retries as usize).min(32)] += 1;
                }
                let dt = t.elapsed().as_nanos() as f64;
                let mut g = hist.lock().unwrap();
                for (a, b) in g.iter_mut().zip(h) {
                    *a += b;
                }
                dt
            }));
        }
        let writer_h = match writer {
            WriterMode::None => None,
            WriterMode::HotKey | WriterMode::LowerHalf => Some(s.spawn(move || {
                let (lo, hi, pace) = match writer {
                    WriterMode::HotKey => (0usize, 1usize, 0u32),
                    _ => (0, NKEYS / 2, 64),
                };
                let names: Vec<Vec<u8>> = (lo..hi).map(key_name).collect();
                let mut rq = crate::seqlock::RetireQueue::new(512);
                let mut rng = 3u64;
                let mut nonce = 0u64;
                let mut writes = 0u64;
                while !stop.load(Ordering::Acquire) {
                    rng = splitmix64(rng);
                    nonce += 1;
                    let ki = (rng % (hi - lo) as u64) as usize;
                    let seed = seed_of((lo + ki) as u64, nonce);
                    let payload = build_payload(seed, PERF_LEN, 0);
                    let old =
                        ks.entry(&names[ki]).write(Value::Str(SmallBytes::from_vec(payload)), 0);
                    rq.retire(&ks.ebr, old);
                    if table_word {
                        tbl.fetch_add(1, Ordering::Release);
                    }
                    writes += 1;
                    for _ in 0..pace {
                        std::hint::spin_loop();
                    }
                }
                rq.drain_all(); // readers already joined when stop fires
                writes
            })),
        };
        let mut total_thread_ns = 0.0;
        for h in handles {
            total_thread_ns += h.join().unwrap();
        }
        stop.store(true, Ordering::Release);
        if let Some(h) = writer_h {
            black_box(h.join().unwrap());
        }
        wall = total_thread_ns;
    });

    let total_reads = ops * r as u64;
    let g = hist.into_inner().unwrap();
    let sum: u64 = g.iter().sum();
    let want = (sum as f64 * 0.99).ceil() as u64;
    let mut acc = 0u64;
    let mut p99 = 32usize;
    for (i, n) in g.iter().enumerate() {
        acc += n;
        if acc >= want {
            p99 = i;
            break;
        }
    }
    Cell {
        // avg thread-time per read (comparable across reader counts)
        ns_per_read: wall / total_reads as f64,
        // aggregate throughput: total reads over avg per-thread wall time
        aggregate_mops: total_reads as f64 * r as f64 * 1e9 / wall / 1e6,
        retry_p99: p99,
    }
}
