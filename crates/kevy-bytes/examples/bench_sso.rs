//! SmallBytes hot-path micro-bench. Pairs each operation against the closest
//! `Vec<u8>` baseline so the ratio is the headline number.
//!
//! `cargo run -p kevy-bytes --example bench_sso --release`

use kevy_bench::{bench, black_box};
use kevy_bytes::SmallBytes;

const SAMPLES: usize = 60;
const INNER: usize = 200_000;

fn main() {
    println!("kevy-bytes SmallBytes micro-bench — vs Vec<u8> baseline");
    println!("(ratios are the signal; absolutes drift on a loaded host)\n");

    // 12-byte payload: inline path (≤ 23 B threshold).
    let short = b"hello world!".to_vec();
    // 64-byte payload: heap path.
    let long: Vec<u8> = (0u8..64).collect();

    println!("== from_slice (construct from &[u8]) ==");
    let sb_short = bench(SAMPLES, INNER, || {
        black_box(SmallBytes::from_slice(black_box(&short)));
    });
    sb_short.report("SmallBytes::from_slice (12B, inline)");
    let vec_short = bench(SAMPLES, INNER, || {
        black_box(Vec::<u8>::from(black_box(short.as_slice())));
    });
    vec_short.report("Vec::from           (12B)         ");
    println!(
        "  ratio (Vec / SmallBytes inline) = {:.2}×\n",
        vec_short.median_ns as f64 / sb_short.median_ns as f64
    );

    let sb_long = bench(SAMPLES, INNER, || {
        black_box(SmallBytes::from_slice(black_box(&long)));
    });
    sb_long.report("SmallBytes::from_slice (64B, heap) ");
    let vec_long = bench(SAMPLES, INNER, || {
        black_box(Vec::<u8>::from(black_box(long.as_slice())));
    });
    vec_long.report("Vec::from           (64B)         ");

    println!("\n== clone (deep copy) ==");
    let sb12 = SmallBytes::from_slice(&short);
    let vec12 = short.clone();
    let cl_sb = bench(SAMPLES, INNER, || {
        black_box(black_box(&sb12).clone());
    });
    cl_sb.report("SmallBytes clone (12B, inline)    ");
    let cl_vec = bench(SAMPLES, INNER, || {
        black_box(black_box(&vec12).clone());
    });
    cl_vec.report("Vec clone        (12B)            ");
    println!(
        "  ratio (Vec / SmallBytes inline) = {:.2}×\n",
        cl_vec.median_ns as f64 / cl_sb.median_ns as f64
    );

    println!("== as_slice (read borrow; no alloc) ==");
    let sb_as = bench(SAMPLES, INNER, || {
        black_box(black_box(&sb12).as_slice());
    });
    sb_as.report("SmallBytes as_slice (inline)      ");
    let v_as = bench(SAMPLES, INNER, || {
        black_box(black_box(&vec12).as_slice());
    });
    v_as.report("Vec as_slice                      ");

    println!("\n== len ==");
    let sb_len = bench(SAMPLES, INNER, || {
        black_box(black_box(&sb12).len());
    });
    sb_len.report("SmallBytes len   (inline)         ");
    let v_len = bench(SAMPLES, INNER, || {
        black_box(black_box(&vec12).len());
    });
    v_len.report("Vec len                           ");
}
