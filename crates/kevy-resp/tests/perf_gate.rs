//! Regression budgets for the per-command RESP codec.
//!
//! Dev profile (unoptimised) + shared host, so budgets carry large headroom:
//! they catch order-of-magnitude regressions (an accidental O(n²) scan, a
//! per-arg double-allocation), not nanosecond drift.

#![cfg(not(debug_assertions))]

use std::hint::black_box;
use std::time::Duration;

use kevy_bench::time_median;
use kevy_resp::{encode_bulk, encode_integer, encode_simple_string, parse_command};

const ITERS: usize = 2_000;

#[test]
fn parse_set_under_budget() {
    let set = b"*3\r\n$3\r\nSET\r\n$5\r\nkey42\r\n$16\r\nvalue-payload-16\r\n";
    let median = time_median(ITERS, || {
        black_box(parse_command(black_box(set)).unwrap());
    });
    // Budget 10 µs (release ~70 ns, alloc-bound; dev ~300 ns–1.5 µs).
    let budget = Duration::from_micros(10);
    assert!(median < budget, "parse_set median {median:?} > {budget:?}");
}

#[test]
fn parse_inline_under_budget() {
    let ping = b"PING\r\n";
    let median = time_median(ITERS, || {
        black_box(parse_command(black_box(ping)).unwrap());
    });
    let budget = Duration::from_micros(5);
    assert!(median < budget, "parse_inline median {median:?} > {budget:?}");
}

#[test]
fn encoders_under_budget() {
    let mut out = Vec::with_capacity(64);
    let median = time_median(ITERS, || {
        out.clear();
        encode_bulk(&mut out, black_box(b"value-payload-16"));
        encode_simple_string(&mut out, black_box("OK"));
        encode_integer(&mut out, black_box(12_345));
    });
    // Three encodes into a reused buffer; release ~15 ns total.
    let budget = Duration::from_micros(5);
    assert!(median < budget, "encoders median {median:?} > {budget:?}");
}
