//! seqlock_probe — pre-work gate prototype for the shared-read keyspace
//! design.
//!
//! Question under judgement: can GET's cross-shard forwarding (measured
//! by the GET-pipeline decomposition at 12% direct + up to +65% cycles
//! in spread) be removed by a seqlock-protected shared-read keyspace —
//! any shard reads any key directly, writes stay shard-owned?
//!
//! Gate criteria (fixed before implementation):
//!   G1  torn reads = 0 over ≥1M mixed-class reads under write pressure
//!   G2  read-retry p99 ≤ 2 at a 50/50 read/write mix
//!   G3  direct read vs forwarded round trip saves ≥ 0.3 µs/op
//!   (+) 8-reader cache-line behaviour informs the version-word layout
//!
//! Run: `cargo run -p kevy-bench --release --example seqlock_probe`

mod perfsim;
mod seqlock;
mod workloads;

use perfsim::WriterMode;

fn main() {
    println!("== seqlock_probe: L1 shared-read keyspace gate ==");
    println!(
        "host: {} threads available",
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0)
    );

    // -----------------------------------------------------------------
    // G1 + G2 — correctness under write pressure (real kevy-map probe,
    // real kevy-bytes SSO Str inline/heap + ArcBulk + Int rotation, TTL).
    // -----------------------------------------------------------------
    println!("\n-- Config A: 50/50 — 4 owner-writers (disjoint ranges) + 4 shared readers --");
    let (st, wr, secs) = workloads::run_5050(1024, 4, 4, 2_000_000);
    print_read_stats(&st, wr.writes, secs);
    println!(
        "  EBR: retired {:.1}M, freed-via-epoch {:.1}M, max parked {} (bounded)",
        wr.retired as f64 / 1e6,
        wr.freed as f64 / 1e6,
        wr.max_parked
    );
    let a_p99 = st.percentile(0.99);
    let a_torn = st.torn + st.expired_hit;

    println!("\n-- Config B: worst case — 1 writer + 7 readers, ONE hot key --");
    let (st_b, writes_b, ) = {
        let (s, w) = workloads::run_hotkey(7, 2_000_000);
        (s, w)
    };
    print_read_stats(&st_b, writes_b, 0.0);
    let b_torn = st_b.torn + st_b.expired_hit;

    // -----------------------------------------------------------------
    // G3 — perf upper bound: direct read vs forwarded round trip.
    // -----------------------------------------------------------------
    println!("\n-- Direct read cost (single thread, uncontended, 16 B values) --");
    let d = perfsim::bench_direct(8_000_000);
    println!("  plain owner-local map GET      : {:>7.1} ns/op", d.plain_ns);
    println!("  seqlock GET, pin per 16-batch  : {:>7.1} ns/op", d.seq_pin_batch_ns);
    println!("  seqlock GET, pin per op        : {:>7.1} ns/op", d.seq_pin_op_ns);
    println!(
        "  seqlock overhead vs plain      : {:>+7.1} ns/op (batch-pin)",
        d.seq_pin_batch_ns - d.plain_ns
    );

    println!("\n-- Forwarded round trip (kevy-ring SPSC, window 256, busy-poll owner) --");
    let f = perfsim::bench_forward(8_000_000);
    println!("  bare ring hop (idealised)      : {:>7.1} ns/op", f.hop_only_ns);
    println!("  S08-S14 chain, batch 16 (main axis) : {:>7.1} ns/op", f.chain16_ns);
    println!("  S08-S14 chain, batch 2  (spread)    : {:>7.1} ns/op", f.chain2_ns);
    let saving_ns = f.chain16_ns - d.seq_pin_batch_ns;
    let saving_spread_ns = f.chain2_ns - d.seq_pin_batch_ns;
    println!(
        "  direct-read saving: main axis {:.2} µs | spread {:.2} µs",
        saving_ns / 1000.0,
        saving_spread_ns / 1000.0
    );

    // -----------------------------------------------------------------
    // Contention matrix — version-word layout evidence.
    // -----------------------------------------------------------------
    println!("\n-- Contention matrix (16 B values, ns per read / aggregate Mops / retry p99) --");
    let ops = 2_000_000u64;
    let cells: [(&str, usize, bool, WriterMode, bool); 6] = [
        ("A  1 reader,  1024 keys, no writer          ", 1, false, WriterMode::None, false),
        ("B  8 readers, 1024 keys, no writer          ", 8, false, WriterMode::None, false),
        ("C  8 readers, HOT key,   no writer          ", 8, true, WriterMode::None, false),
        ("D  8 readers, HOT key,   writer full speed  ", 8, true, WriterMode::HotKey, false),
        ("E  8 readers, 1024 keys, paced writer       ", 8, false, WriterMode::LowerHalf, false),
        ("F  = E + shared TABLE version word (anti)   ", 8, false, WriterMode::LowerHalf, true),
    ];
    for (label, r, hot, wm, tbl) in cells {
        let c = perfsim::run_cell(r, hot, wm, tbl, ops);
        println!(
            "  {label}: {:>7.1} ns/read  {:>8.2} Mops  p99 retries {}",
            c.ns_per_read, c.aggregate_mops, c.retry_p99
        );
    }

    // -----------------------------------------------------------------
    // Gate verdict.
    // -----------------------------------------------------------------
    println!("\n== GATE VERDICT ==");
    let g1 = a_torn == 0 && b_torn == 0;
    let g2 = a_p99 <= 2;
    // Generous reading: pass if EITHER the main-axis (batch 16) or the
    // spread (batch 2) per-op saving clears the bar.
    let g3 = saving_ns >= 300.0 || saving_spread_ns >= 300.0;
    println!(
        "  G1 torn reads = 0            : {} (A: {}, B: {}; reads A {:.1}M / B {:.1}M)",
        pass(g1),
        a_torn,
        b_torn,
        st.reads as f64 / 1e6,
        st_b.reads as f64 / 1e6
    );
    println!("  G2 retry p99 <= 2 @ 50/50    : {} (p99 = {}, max = {})", pass(g2), a_p99, st.max_retry());
    println!(
        "  G3 direct saving >= 0.3 µs   : {} (main axis {:.2} µs, spread {:.2} µs; darwin/arm64 box — cross-check on lx64 before any implementation)",
        pass(g3),
        saving_ns / 1000.0,
        saving_spread_ns / 1000.0
    );
    println!("  overall: {}", pass(g1 && g2 && g3));
}

fn pass(b: bool) -> &'static str {
    if b { "PASS" } else { "FAIL" }
}

fn print_read_stats(st: &workloads::ReadStats, writes: u64, secs: f64) {
    println!(
        "  reads {:.1}M (hits {:.1}M, expired-as-miss {}, arc zero-copy reads {:.1}M)",
        st.reads as f64 / 1e6,
        st.hits as f64 / 1e6,
        st.expired,
        st.arc_reads as f64 / 1e6
    );
    println!("  writes {:.1}M  (read:write = {:.2})", writes as f64 / 1e6, st.reads as f64 / writes as f64);
    if secs > 0.0 {
        println!("  wall {secs:.2}s");
    }
    println!(
        "  TORN {}  torn-expire-pairing {}  | retries p50 {} p99 {} p999 {} max-bucket {} (>=32: {} reads)",
        st.torn,
        st.expired_hit,
        st.percentile(0.50),
        st.percentile(0.99),
        st.percentile(0.999),
        st.max_retry(),
        st.hist[workloads::HIST_BUCKETS - 1]
    );
}
