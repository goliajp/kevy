//! R2c steady-state probe: the window slide's cost against churn rate
//! and window size — the criterion says cost tracks the eviction RATE
//! and ignores the window's SIZE, with a near-zero idle tick.
//!
//! Manual-mode ticks make the measurement deterministic: each sample
//! is one `Store::tick()` under a controlled write batch. Shape (not
//! absolute µs) is the finding; medians over repeated rounds.
//!
//! Run: `cargo run --release -p kevy-embedded --example diag_window_steady`

use std::time::Instant;

use kevy_embedded::{Config, Store};
use kevy_index::{IndexKind, TableIndex, TableSpec, ValType, WindowSpec};

fn windowed_table(span: i64, bucket: i64) -> TableSpec {
    TableSpec {
        name: b"ev".to_vec(),
        prefix: b"ev:".to_vec(),
        pk: b"id".to_vec(),
        columns: vec![(b"id".to_vec(), ValType::Str), (b"at".to_vec(), ValType::I64)],
        indexes: vec![TableIndex {
            column: b"at".to_vec(),
            kind: IndexKind::Range,
            values: vec![],
        }],
        orderpaths: vec![],
        window: Some(WindowSpec { column: b"at".to_vec(), span, bucket }),
        autodeclare: 0,
        auto_added: vec![],
    }
}

struct Probe {
    s: Store,
    _dir: kevy_tmpdir::TmpDir,
    next_at: i64,
    next_id: u64,
}

impl Probe {
    fn new(span: i64, bucket: i64) -> Self {
        let dir = kevy_tmpdir::TmpDir::new("diag-winsteady");
        let s = Store::open(Config::default().with_persist(dir.path()).with_ttl_reaper_manual())
            .expect("open");
        s.table_declare(windowed_table(span, bucket)).expect("declare");
        Self { s, _dir: dir, next_at: 0, next_id: 0 }
    }

    /// Write `n` rows, each advancing the window column by one unit.
    fn churn(&mut self, n: usize) {
        for _ in 0..n {
            let key = format!("ev:{}", self.next_id);
            let argv: Vec<Vec<u8>> = vec![
                b"HSET".to_vec(),
                key.clone().into_bytes(),
                b"id".to_vec(),
                key.into_bytes(),
                b"at".to_vec(),
                self.next_at.to_string().into_bytes(),
            ];
            let mut out = Vec::new();
            self.s.dispatch_argv(&argv, &mut out);
            self.next_id += 1;
            self.next_at += 1;
        }
    }

    /// One measured tick, in microseconds.
    fn tick_us(&self) -> f64 {
        let t = Instant::now();
        self.s.tick();
        t.elapsed().as_secs_f64() * 1e6
    }
}

/// Median tick µs over `rounds` ticks, each preceded by a `churn`-row
/// write batch. The warmup fills the window past its span so every
/// measured tick slides in steady state.
fn steady_median(span: i64, bucket: i64, churn: usize, rounds: usize) -> f64 {
    let mut p = Probe::new(span, bucket);
    // Fill past 2× span, ticking as a background cadence would.
    while p.next_at < span * 2 {
        p.churn(churn.max(64));
        p.s.tick();
    }
    let mut samples: Vec<f64> = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        p.churn(churn);
        samples.push(p.tick_us());
    }
    samples.sort_by(|a, b| a.total_cmp(b));
    samples[samples.len() / 2]
}

/// Median idle tick µs (window full, zero churn).
fn idle_median(span: i64, bucket: i64, rounds: usize) -> f64 {
    let mut p = Probe::new(span, bucket);
    while p.next_at < span * 2 {
        p.churn(64);
        p.s.tick();
    }
    p.s.tick();
    let mut samples: Vec<f64> = (0..rounds).map(|_| p.tick_us()).collect();
    samples.sort_by(|a, b| a.total_cmp(b));
    samples[samples.len() / 2]
}

fn main() {
    const ROUNDS: usize = 100;
    println!("# window slide steady-state (median tick µs, {ROUNDS} rounds, 3 runs each)");
    println!("# every measured tick slides ≥1 bucket (churn ≥ bucket), so the median IS the slide\n");

    println!("## axis 1 — window SIZE at fixed bucket(100)/churn(128); flat = size-independent");
    for span in [1_000i64, 10_000, 100_000] {
        let m: Vec<f64> = (0..3).map(|_| steady_median(span, 100, 128, ROUNDS)).collect();
        println!("span {span:>7}: {:?}", m.iter().map(|v| v.round()).collect::<Vec<_>>());
    }

    println!("\n## axis 2 — churn RATE at fixed span(10_000)/bucket(100); slides/tick = churn/100");
    for churn in [128usize, 256, 512, 1024] {
        let m: Vec<f64> = (0..3).map(|_| steady_median(10_000, 100, churn, ROUNDS)).collect();
        println!("churn {churn:>4}/tick: {:?}", m.iter().map(|v| v.round()).collect::<Vec<_>>());
    }

    println!("\n## axis 2b — BUCKET amortization at fixed span(20_000)/churn(512); cost ∝ 1/bucket");
    for bucket in [100i64, 500, 2_000] {
        let m: Vec<f64> = (0..3).map(|_| steady_median(20_000, bucket, 512, ROUNDS)).collect();
        println!("bucket {bucket:>5}: {:?}", m.iter().map(|v| v.round()).collect::<Vec<_>>());
    }

    println!("\n## axis 3 — idle tick (window full, zero churn); near-zero expected");
    for span in [1_000i64, 100_000] {
        let m: Vec<f64> = (0..3).map(|_| idle_median(span, span / 100, ROUNDS)).collect();
        println!("span {span:>7}: {:?}", m.iter().map(|v| v.round()).collect::<Vec<_>>());
    }
}
