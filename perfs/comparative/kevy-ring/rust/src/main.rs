//! Cross-competitor Rust SPSC bench for kevy-ring.
//!
//! Workloads:
//! - `push_pop_same_thread_u64` — single-thread push followed by pop (the
//!   hot inner loop primitive; measures pure cursor + slot mechanics)
//! - `spsc_throughput_cap1024_u64` — true cross-thread SPSC, throughput
//!   in ns/item moving N items end-to-end
//!
//! Competitors:
//! - `kevy-ring`            — the stone under test
//! - `rtrb` 0.3             — the standalone SPSC default in the Rust eco
//! - `ringbuf` 0.4          — another popular SPSC
//! - `crossbeam::ArrayQueue` 0.3 — MPMC bounded queue (degrades to SPSC; the
//!                                Rust default when reaching for "bounded
//!                                lock-free queue" without a specific
//!                                SPSC ask)
//!
//! Same JSON schema as the other comparatives (see
//! perfs/comparative/README.md).

use crossbeam_queue::ArrayQueue;
use kevy_ring::ring as kevy_ring;
use std::hint::black_box;
use std::sync::Arc;
use std::thread;
use std::time::Instant;

const ITER_INNER: usize = 1_000_000;
const SAMPLES: usize = 25;
const CROSS_N: usize = 4_000_000;
const HOST: &str = "M4-Pro-aarch64";
const STONE: &str = "kevy-ring";

fn now_iso() -> String {
    std::process::Command::new("date")
        .args(["-u", "-Iseconds"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn percentiles(times: &mut Vec<u64>) -> (u64, u64, u64) {
    times.sort_unstable();
    let n = times.len();
    (times[n / 2], times[(n * 95) / 100], times[0])
}

fn emit_json(competitor: &str, workload: &str, m: u64, p95: u64, min: u64, iters: usize) {
    println!(
        "{{\"stone\":\"{STONE}\",\"language\":\"rust\",\"competitor\":\"{competitor}\",\"workload\":\"{workload}\",\"metric\":\"ns_per_op\",\"value_median\":{m},\"value_p95\":{p95},\"value_min\":{min},\"iterations\":{iters},\"host\":\"{HOST}\",\"date\":\"{}\"}}",
        now_iso()
    );
}

fn time_one<F: FnMut()>(iter: usize, mut f: F) -> u64 {
    let t = Instant::now();
    for _ in 0..iter {
        f();
    }
    (t.elapsed().as_nanos() as u64) / iter as u64
}

fn bench_same_thread<F: FnMut()>(competitor: &str, workload: &str, mut f: F) {
    let mut times = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        times.push(time_one(ITER_INNER, &mut f));
    }
    let (m, p95, min) = percentiles(&mut times);
    emit_json(competitor, workload, m, p95, min, ITER_INNER);
}

fn bench_cross_thread<P, C>(competitor: &str, workload: &str, mut producer: P, mut consumer: C)
where
    P: FnMut(usize) + Send + 'static,
    C: FnMut(usize) -> u64 + Send + 'static,
{
    let mut times = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let t = Instant::now();
        let prod_handle = {
            let mut p = unsafe { core::mem::replace(&mut producer, core::mem::zeroed()) };
            thread::spawn(move || p(CROSS_N))
        };
        let cons_handle = {
            let mut c = unsafe { core::mem::replace(&mut consumer, core::mem::zeroed()) };
            thread::spawn(move || c(CROSS_N))
        };
        let _sum = cons_handle.join().unwrap();
        prod_handle.join().unwrap();
        let ns = t.elapsed().as_nanos() as u64;
        times.push(ns / CROSS_N as u64);
    }
    let (m, p95, min) = percentiles(&mut times);
    emit_json(competitor, workload, m, p95, min, CROSS_N);
}

fn main() {
    // ---- push + pop same thread (u64, cap=1024) ----
    // (kevy-ring)
    {
        let (mut tx, mut rx) = kevy_ring::<u64>(1024);
        bench_same_thread("kevy-ring", "push_pop_same_thread_u64", || {
            let _ = tx.push(black_box(0xdeadbeefu64));
            let _ = black_box(rx.pop());
        });
    }
    {
        let (mut prod, mut cons) = rtrb::RingBuffer::<u64>::new(1024);
        bench_same_thread("rtrb", "push_pop_same_thread_u64", || {
            let _ = prod.push(black_box(0xdeadbeefu64));
            let _ = black_box(cons.pop());
        });
    }
    {
        use ringbuf::traits::{Consumer, Producer, Split};
        let rb = ringbuf::HeapRb::<u64>::new(1024);
        let (mut prod, mut cons) = rb.split();
        bench_same_thread("ringbuf", "push_pop_same_thread_u64", || {
            let _ = prod.try_push(black_box(0xdeadbeefu64));
            let _ = black_box(cons.try_pop());
        });
    }
    {
        let q = ArrayQueue::<u64>::new(1024);
        bench_same_thread("crossbeam::ArrayQueue", "push_pop_same_thread_u64", || {
            let _ = q.push(black_box(0xdeadbeefu64));
            let _ = black_box(q.pop());
        });
    }

    // ---- cross-thread SPSC throughput (cap=1024, N=4M items) ----
    // Each call constructs producer + consumer closures specialised to the
    // ring type; CROSS_N items are pushed by one thread and popped by
    // another. ns/item is end-to-end.

    // kevy-ring
    {
        let mut times = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let (mut tx, mut rx) = kevy_ring::<u64>(1024);
            let t = Instant::now();
            let p = thread::spawn(move || {
                for i in 0..CROSS_N as u64 {
                    while tx.push(i).is_err() {
                        std::hint::spin_loop();
                    }
                }
            });
            let c = thread::spawn(move || {
                let mut sum: u64 = 0;
                for _ in 0..CROSS_N {
                    loop {
                        if let Some(v) = rx.pop() {
                            sum = sum.wrapping_add(v);
                            break;
                        }
                        std::hint::spin_loop();
                    }
                }
                sum
            });
            let _ = c.join().unwrap();
            p.join().unwrap();
            times.push((t.elapsed().as_nanos() as u64) / CROSS_N as u64);
        }
        let (m, p95, min) = percentiles(&mut times);
        emit_json("kevy-ring", "spsc_cap1024_u64", m, p95, min, CROSS_N);
    }

    // rtrb
    {
        let mut times = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let (mut prod, mut cons) = rtrb::RingBuffer::<u64>::new(1024);
            let t = Instant::now();
            let p = thread::spawn(move || {
                for i in 0..CROSS_N as u64 {
                    while prod.push(i).is_err() {
                        std::hint::spin_loop();
                    }
                }
            });
            let c = thread::spawn(move || {
                let mut sum: u64 = 0;
                for _ in 0..CROSS_N {
                    loop {
                        if let Ok(v) = cons.pop() {
                            sum = sum.wrapping_add(v);
                            break;
                        }
                        std::hint::spin_loop();
                    }
                }
                sum
            });
            let _ = c.join().unwrap();
            p.join().unwrap();
            times.push((t.elapsed().as_nanos() as u64) / CROSS_N as u64);
        }
        let (m, p95, min) = percentiles(&mut times);
        emit_json("rtrb", "spsc_cap1024_u64", m, p95, min, CROSS_N);
    }

    // ringbuf
    {
        use ringbuf::traits::{Consumer, Producer, Split};
        let mut times = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let rb = ringbuf::HeapRb::<u64>::new(1024);
            let (mut prod, mut cons) = rb.split();
            let t = Instant::now();
            let p = thread::spawn(move || {
                for i in 0..CROSS_N as u64 {
                    while prod.try_push(i).is_err() {
                        std::hint::spin_loop();
                    }
                }
            });
            let c = thread::spawn(move || {
                let mut sum: u64 = 0;
                for _ in 0..CROSS_N {
                    loop {
                        if let Some(v) = cons.try_pop() {
                            sum = sum.wrapping_add(v);
                            break;
                        }
                        std::hint::spin_loop();
                    }
                }
                sum
            });
            let _ = c.join().unwrap();
            p.join().unwrap();
            times.push((t.elapsed().as_nanos() as u64) / CROSS_N as u64);
        }
        let (m, p95, min) = percentiles(&mut times);
        emit_json("ringbuf", "spsc_cap1024_u64", m, p95, min, CROSS_N);
    }

    // crossbeam::ArrayQueue
    {
        let mut times = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let q = Arc::new(ArrayQueue::<u64>::new(1024));
            let qp = Arc::clone(&q);
            let qc = q;
            let t = Instant::now();
            let p = thread::spawn(move || {
                for i in 0..CROSS_N as u64 {
                    while qp.push(i).is_err() {
                        std::hint::spin_loop();
                    }
                }
            });
            let c = thread::spawn(move || {
                let mut sum: u64 = 0;
                for _ in 0..CROSS_N {
                    loop {
                        if let Some(v) = qc.pop() {
                            sum = sum.wrapping_add(v);
                            break;
                        }
                        std::hint::spin_loop();
                    }
                }
                sum
            });
            let _ = c.join().unwrap();
            p.join().unwrap();
            times.push((t.elapsed().as_nanos() as u64) / CROSS_N as u64);
        }
        let (m, p95, min) = percentiles(&mut times);
        emit_json(
            "crossbeam::ArrayQueue",
            "spsc_cap1024_u64",
            m,
            p95,
            min,
            CROSS_N,
        );
    }
}
