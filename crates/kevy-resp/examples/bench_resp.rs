//! RESP codec micro-bench — the per-command parse + reply-encode hot path.
//! `cargo run -p kevy-resp --example bench_resp --release`
//!
//! Ratios/absolutes are indicative on a loaded host; the point is to see where
//! the per-command codec time goes (parse allocates the owned argv that the
//! thread-per-core runtime forwards cross-core; encoders append to a reused
//! buffer and should be near-free).

use kevy_bench::{bench, black_box, report};
use kevy_resp::{
    encode_bulk, encode_integer, encode_simple_string, parse_command,
};

const SAMPLES: usize = 60;
const INNER: usize = 50_000;

fn main() {
    let get = b"*2\r\n$3\r\nGET\r\n$5\r\nkey42\r\n".to_vec();
    let set = b"*3\r\n$3\r\nSET\r\n$5\r\nkey42\r\n$16\r\nvalue-payload-16\r\n".to_vec();
    let ping = b"PING\r\n".to_vec();

    println!("kevy-resp codec micro-bench (ratios/absolutes indicative under host load)\n");
    println!("== parse_command (allocates owned argv) ==");
    let g = bench(SAMPLES, INNER, || {
        black_box(parse_command(black_box(&get)).unwrap());
    });
    report("parse GET (2 args)", g);
    let s = bench(SAMPLES, INNER, || {
        black_box(parse_command(black_box(&set)).unwrap());
    });
    report("parse SET (3 args)", s);
    let p = bench(SAMPLES, INNER, || {
        black_box(parse_command(black_box(&ping)).unwrap());
    });
    report("parse PING (inline)", p);

    println!("\n== reply encoders (append to a reused buffer) ==");
    let mut out = Vec::with_capacity(64);
    let eb = bench(SAMPLES, INNER, || {
        out.clear();
        encode_bulk(&mut out, black_box(b"value-payload-16"));
        black_box(&out);
    });
    report("encode_bulk", eb);
    let es = bench(SAMPLES, INNER, || {
        out.clear();
        encode_simple_string(&mut out, black_box("OK"));
        black_box(&out);
    });
    report("encode_simple_string", es);
    let ei = bench(SAMPLES, INNER, || {
        out.clear();
        encode_integer(&mut out, black_box(12_345));
        black_box(&out);
    });
    report("encode_integer", ei);
}
