//! kevy-uring nop round-trip latency bench.
//!
//! Measures the minimal `prep_nop → submit_and_wait → for_each_completion`
//! round-trip ns. This is the kernel-floor of io_uring: any wrapper must
//! reach this number (the syscall + cursor advance), no library should be
//! materially slower. Compared cross-language to C liburing in
//! `../c/bench.c`.

use std::hint::black_box;
use std::time::Instant;

use kevy_uring::IoUring;

const SAMPLES: usize = 25;
const INNER: usize = 100_000;
const STONE: &str = "kevy-uring";

fn now_iso() -> String {
    std::process::Command::new("date")
        .args(["-u", "-Iseconds"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn host() -> String {
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;
    format!("{os}-{arch}")
}

fn percentiles(times: &mut [u64]) -> (u64, u64, u64) {
    times.sort_unstable();
    let n = times.len();
    (times[n / 2], times[(n * 95) / 100], times[0])
}

fn emit(competitor: &str, workload: &str, med: u64, p95: u64, min: u64, iters: usize) {
    println!(
        "{{\"stone\":\"{STONE}\",\"language\":\"rust\",\"competitor\":\"{competitor}\",\"workload\":\"{workload}\",\"metric\":\"ns_per_op\",\"value_median\":{med},\"value_p95\":{p95},\"value_min\":{min},\"iterations\":{iters},\"host\":\"{}\",\"date\":\"{}\"}}",
        host(),
        now_iso()
    );
}

fn bench_nop_rtt() {
    let mut ring = IoUring::new(32).expect("ring");
    let mut times = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let t = Instant::now();
        for _ in 0..INNER {
            // SQ has cap 32; one in-flight at a time so it never fills.
            assert!(ring.prep_nop(0));
            black_box(ring.submit_and_wait(1).unwrap());
            ring.for_each_completion(|_| {});
        }
        let ns = t.elapsed().as_nanos() as u64;
        times.push(ns / INNER as u64);
    }
    let (med, p95, min) = percentiles(&mut times);
    emit("kevy-uring", "nop_rtt", med, p95, min, INNER);
}

fn main() {
    bench_nop_rtt();
}
