//! perf-record harness for the SET scalar path (mmkvgate decomposition).
//! Loops Store::set of a fixed-size value over a small warm keyspace, so
//! `perf record` attributes self-time across the SET path: value copy,
//! AOF write_multibulk (fmt machinery), BufWriter, hashmap insert.
//!
//!   cargo build --release --example set4k_bench
//!   perf record -g --call-graph dwarf \
//!     target/release/examples/set4k_bench /tmp/b 4096 4000000
//!   perf report --stdio | head -40
use kevy_embedded::{Config, Store};

fn main() {
    let mut a = std::env::args().skip(1);
    let dir = a.next().expect("dir");
    let vsize: usize = a.next().and_then(|s| s.parse().ok()).unwrap_or(4096);
    let n: u64 = a.next().and_then(|s| s.parse().ok()).unwrap_or(4_000_000);
    // 4th arg: aof 1/0. AOF-off isolates the store path (value.to_vec +
    // hashmap insert) from the AOF append (write_multibulk + syscall), so
    // the A/B pins which side owns the SET cost.
    let aof = a.next().map_or(true, |s| s != "0");
    let cfg = if aof { Config::default().with_persist(dir) } else { Config::default() };
    let store = Store::open(cfg).expect("open");
    let val = vec![0x61u8; vsize];
    let keys: Vec<Vec<u8>> = (0..200).map(|i| format!("k{i}").into_bytes()).collect();
    for i in 0..n {
        store.set(&keys[(i % 200) as usize], &val).expect("set");
    }
    println!("done {n} sets of {vsize}B");
}
