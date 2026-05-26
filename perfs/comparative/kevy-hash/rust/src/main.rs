//! Cross-competitor Rust micro-bench for kevy-hash.
//!
//! Competitors: ahash, rustc-hash, fxhash (legacy), seahash, std SipHash.
//! Workloads: hash an 8/16/64-byte byte string + a u64 integer.
//! Schema: see `perfs/comparative/README.md`. JSON lines to stdout.

use ahash::AHasher;
use fxhash::FxHasher as LegacyFxHasher;
use kevy_hash::{FxHasher as KevyFxHasher, KevyHash};
use rustc_hash::FxHasher as RustcFxHasher;
use seahash::SeaHasher;
use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;
use std::hint::black_box;
use std::time::Instant;

const ITER: usize = 1_000_000;
const SAMPLES: usize = 25;
const HOST: &str = "M4-Pro-aarch64";
const STONE: &str = "kevy-hash";

fn now_iso() -> String {
    std::process::Command::new("date")
        .args(["-u", "-Iseconds"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn time_one<F: FnMut() -> u64>(iter: usize, mut f: F) -> u64 {
    let t = Instant::now();
    let mut acc = 0u64;
    for _ in 0..iter {
        acc ^= f();
    }
    let elapsed = t.elapsed().as_nanos() as u64;
    black_box(acc);
    elapsed / iter as u64
}

fn percentiles(times: &mut Vec<u64>) -> (u64, u64, u64) {
    times.sort_unstable();
    let n = times.len();
    (times[n / 2], times[(n * 95) / 100], times[0])
}

fn emit_json(competitor: &str, workload: &str, m: u64, p95: u64, min: u64) {
    println!(
        "{{\"stone\":\"{STONE}\",\"language\":\"rust\",\"competitor\":\"{competitor}\",\"workload\":\"{workload}\",\"metric\":\"ns_per_op\",\"value_median\":{m},\"value_p95\":{p95},\"value_min\":{min},\"iterations\":{ITER},\"host\":\"{HOST}\",\"date\":\"{}\"}}",
        now_iso()
    );
}

fn bench<F: FnMut() -> u64>(competitor: &str, workload: &str, mut f: F) {
    let mut times = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        times.push(time_one(ITER, &mut f));
    }
    let (m, p95, min) = percentiles(&mut times);
    emit_json(competitor, workload, m, p95, min);
}

fn make_buf(len: usize) -> Vec<u8> {
    (0..len as u32).map(|i| (i & 0xFF) as u8).collect()
}

fn bench_bytes(workload: &str, buf: &[u8]) {
    // kevy-hash — the one-call inlinable shortcut
    bench("kevy-hash", workload, || black_box(black_box(buf).kevy_hash()));
    // kevy-hash via the Hasher trait (full state-machine)
    bench(&format!("{workload}_via_hasher"), workload, || {
        let mut h = KevyFxHasher::default();
        h.write(black_box(buf));
        black_box(h.finish())
    });
    // Just the kevy FxHasher state machine (no fmix64 finalize)
    // is exposed via the Hasher trait — same as via_hasher above.
    // Other competitors:
    bench("ahash", workload, || {
        let mut h = AHasher::default();
        h.write(black_box(buf));
        black_box(h.finish())
    });
    bench("rustc-hash", workload, || {
        let mut h = RustcFxHasher::default();
        h.write(black_box(buf));
        black_box(h.finish())
    });
    bench("fxhash (legacy)", workload, || {
        let mut h = LegacyFxHasher::default();
        h.write(black_box(buf));
        black_box(h.finish())
    });
    bench("seahash", workload, || {
        let mut h = SeaHasher::default();
        h.write(black_box(buf));
        black_box(h.finish())
    });
    bench("std SipHash", workload, || {
        let mut h = DefaultHasher::new();
        h.write(black_box(buf));
        black_box(h.finish())
    });
}

fn bench_u64(workload: &str, n: u64) {
    bench("kevy-hash", workload, || black_box(black_box(n).kevy_hash()));
    bench("ahash", workload, || {
        let mut h = AHasher::default();
        h.write_u64(black_box(n));
        black_box(h.finish())
    });
    bench("rustc-hash", workload, || {
        let mut h = RustcFxHasher::default();
        h.write_u64(black_box(n));
        black_box(h.finish())
    });
    bench("fxhash (legacy)", workload, || {
        let mut h = LegacyFxHasher::default();
        h.write_u64(black_box(n));
        black_box(h.finish())
    });
    bench("seahash", workload, || {
        let mut h = SeaHasher::default();
        h.write_u64(black_box(n));
        black_box(h.finish())
    });
    bench("std SipHash", workload, || {
        let mut h = DefaultHasher::new();
        h.write_u64(black_box(n));
        black_box(h.finish())
    });
}

fn main() {
    let b8 = make_buf(8);
    let b16 = make_buf(16);
    let b64 = make_buf(64);

    bench_bytes("hash_bytes_8B", &b8);
    bench_bytes("hash_bytes_16B", &b16);
    bench_bytes("hash_bytes_64B", &b64);
    bench_u64("hash_u64", 0xdead_beef_cafe_babe);
}
