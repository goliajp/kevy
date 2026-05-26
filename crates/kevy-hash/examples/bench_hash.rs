//! kevy-hash micro-bench. vs std SipHash baseline (`std::collections::hash_map::
//! RandomState`'s default) on byte-string + integer keys.
//!
//! `cargo run -p kevy-hash --example bench_hash --release`

use kevy_bench::{bench, black_box};
use kevy_hash::{FxHasher, KevyHash};
use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;

const SAMPLES: usize = 60;
const INNER: usize = 200_000;

fn main() {
    println!("kevy-hash vs std SipHash micro-bench");
    println!("ratios are the signal; absolutes drift on a loaded host\n");

    for &len in &[8usize, 16, 24, 32, 64, 128] {
        let buf: Vec<u8> = (0..len as u8).collect();

        // KevyHash trait (one-call inlinable)
        let kh = bench(SAMPLES, INNER, || {
            black_box(black_box(buf.as_slice()).kevy_hash());
        });

        // FxHasher state-machine equivalent (kevy-hash's Hasher impl)
        let fxh = bench(SAMPLES, INNER, || {
            let mut h = FxHasher::default();
            h.write(black_box(buf.as_slice()));
            black_box(h.finish());
        });

        // std SipHash (DefaultHasher)
        let sip = bench(SAMPLES, INNER, || {
            let mut h = DefaultHasher::new();
            h.write(black_box(buf.as_slice()));
            black_box(h.finish());
        });

        println!(
            "  len={len:3}  KevyHash={ns_k} ns  FxHasher={ns_f} ns  SipHash={ns_s} ns  Sip/Kevy={ratio:.2}×",
            ns_k = kh.median_ns,
            ns_f = fxh.median_ns,
            ns_s = sip.median_ns,
            ratio = sip.median_ns as f64 / kh.median_ns as f64,
        );
    }

    println!("\n== integer keys ==");
    let n: u64 = 0xdead_beef_cafe_babe;
    let kh = bench(SAMPLES, INNER, || {
        black_box(black_box(n).kevy_hash());
    });
    let fxh = bench(SAMPLES, INNER, || {
        let mut h = FxHasher::default();
        h.write_u64(black_box(n));
        black_box(h.finish());
    });
    let sip = bench(SAMPLES, INNER, || {
        let mut h = DefaultHasher::new();
        h.write_u64(black_box(n));
        black_box(h.finish());
    });
    println!(
        "  u64  KevyHash={} ns  FxHasher={} ns  SipHash={} ns  Sip/Kevy={:.2}×",
        kh.median_ns, fxh.median_ns, sip.median_ns,
        sip.median_ns as f64 / kh.median_ns as f64,
    );
}
