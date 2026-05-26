//! Cross-competitor Rust micro-bench for kevy-bytes.
//!
//! Emits one JSON object per (competitor, workload) tuple. The structure
//! is documented in `perfs/comparative/README.md`. Caller (`run.sh`)
//! redirects stdout to `results-<date>.jsonl`.
//!
//! Competitors:
//! - `Vec<u8>` (no SSO baseline)
//! - `String` (UTF-8, no SSO baseline)
//! - `smartstring::alias::String` (SSO ≤ 23 bytes)
//! - `compact_str::CompactString` (SSO ≤ 24 bytes via last-byte tag)
//! - `smol_str::SmolStr` (SSO ≤ 23 bytes, immutable, Arc-shared heap)
//! - `kevy_bytes::SmallBytes` (24-byte SSO byte string)
//!
//! Workloads (always run on both 12-byte = inline and 64-byte = heap inputs):
//! - `clone` — clone an owned value
//! - `eq` — compare two equal values
//! - `from_bytes` — construct from a `&[u8]`
//! - `from_str` (string-typed only) — construct from a `&str`

use compact_str::CompactString;
use kevy_bytes::SmallBytes;
use smartstring::alias::String as SmartString;
use smol_str::SmolStr;
use std::hint::black_box;
use std::time::Instant;

const ITER: usize = 1_000_000;
const SAMPLES: usize = 25;

const HOST: &str = "M4-Pro-aarch64";
const STONE: &str = "kevy-bytes";

fn now_iso() -> String {
    // ISO 8601 in local time — emit only the second-resolution to match the
    // baseline schema. We don't need exact ms for stone snapshots.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    // crude: format as YYYY-MM-DDTHH:MM:SSZ via /bin/date for portability.
    let out = std::process::Command::new("date")
        .args(["-u", "-Iseconds"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| format!("{secs}"));
    out
}

fn percentiles(times: &mut Vec<u64>) -> (u64, u64, u64) {
    times.sort_unstable();
    let n = times.len();
    let median = times[n / 2];
    let p95 = times[(n * 95) / 100];
    let min = times[0];
    (median, p95, min)
}

fn emit_json(
    competitor: &str,
    language: &str,
    workload: &str,
    median: u64,
    p95: u64,
    min: u64,
    iterations: usize,
) {
    println!(
        "{{\"stone\":\"{STONE}\",\"language\":\"{language}\",\"competitor\":\"{competitor}\",\"workload\":\"{workload}\",\"metric\":\"ns_per_op\",\"value_median\":{median},\"value_p95\":{p95},\"value_min\":{min},\"iterations\":{iterations},\"host\":\"{HOST}\",\"date\":\"{}\"}}",
        now_iso()
    );
}

/// Run `f` `iter` times, return ns/op for this sample.
fn time_one<F: FnMut()>(iter: usize, mut f: F) -> u64 {
    let t = Instant::now();
    for _ in 0..iter {
        f();
    }
    let elapsed = t.elapsed().as_nanos() as u64;
    elapsed / iter as u64
}

fn bench<F: FnMut()>(competitor: &str, workload: &str, mut f: F) {
    let mut times = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        times.push(time_one(ITER, &mut f));
    }
    let (med, p95, min) = percentiles(&mut times);
    emit_json(competitor, "rust", workload, med, p95, min, ITER);
}

fn main() {
    // Inline-zone (12 bytes) — short of every competitor's SSO threshold.
    let short_b: &[u8] = b"hello world!";
    let short_s: &str = "hello world!";
    // Heap-zone (64 bytes) — past every competitor's SSO threshold; forces
    // allocation on each construction.
    let long_b: Vec<u8> = (0u8..64).collect();
    let long_s: String = "a".repeat(64);

    // ---- clone (inline / 12B) ----
    {
        let src = SmallBytes::from_slice(short_b);
        bench("kevy-bytes", "clone_inline_12B", || {
            black_box(src.clone());
        });
        let src = Vec::<u8>::from(short_b);
        bench("Vec<u8>", "clone_inline_12B", || {
            black_box(src.clone());
        });
        let src = std::string::String::from(short_s);
        bench("std::String", "clone_inline_12B", || {
            black_box(src.clone());
        });
        let src: SmartString = short_s.into();
        bench("smartstring", "clone_inline_12B", || {
            black_box(src.clone());
        });
        let src: CompactString = short_s.into();
        bench("compact_str", "clone_inline_12B", || {
            black_box(src.clone());
        });
        let src: SmolStr = short_s.into();
        bench("smol_str", "clone_inline_12B", || {
            black_box(src.clone());
        });
    }

    // ---- clone (heap / 64B) ----
    {
        let src = SmallBytes::from_slice(&long_b);
        bench("kevy-bytes", "clone_heap_64B", || {
            black_box(src.clone());
        });
        let src = long_b.clone();
        bench("Vec<u8>", "clone_heap_64B", || {
            black_box(src.clone());
        });
        let src = long_s.clone();
        bench("std::String", "clone_heap_64B", || {
            black_box(src.clone());
        });
        let src: SmartString = long_s.as_str().into();
        bench("smartstring", "clone_heap_64B", || {
            black_box(src.clone());
        });
        let src: CompactString = long_s.as_str().into();
        bench("compact_str", "clone_heap_64B", || {
            black_box(src.clone());
        });
        let src: SmolStr = long_s.as_str().into();
        bench("smol_str", "clone_heap_64B", || {
            black_box(src.clone());
        });
    }

    // ---- eq (12B) ----
    {
        let a = SmallBytes::from_slice(short_b);
        let b = SmallBytes::from_slice(short_b);
        bench("kevy-bytes", "eq_inline_12B", || {
            black_box(a.as_slice() == b.as_slice());
        });
        let a = Vec::<u8>::from(short_b);
        let b = Vec::<u8>::from(short_b);
        bench("Vec<u8>", "eq_inline_12B", || {
            black_box(&a == &b);
        });
        let a = std::string::String::from(short_s);
        let b = std::string::String::from(short_s);
        bench("std::String", "eq_inline_12B", || {
            black_box(&a == &b);
        });
        let a: SmartString = short_s.into();
        let b: SmartString = short_s.into();
        bench("smartstring", "eq_inline_12B", || {
            black_box(&a == &b);
        });
        let a: CompactString = short_s.into();
        let b: CompactString = short_s.into();
        bench("compact_str", "eq_inline_12B", || {
            black_box(&a == &b);
        });
        let a: SmolStr = short_s.into();
        let b: SmolStr = short_s.into();
        bench("smol_str", "eq_inline_12B", || {
            black_box(&a == &b);
        });
    }

    // ---- eq (64B) ----
    {
        let a = SmallBytes::from_slice(&long_b);
        let b = SmallBytes::from_slice(&long_b);
        bench("kevy-bytes", "eq_heap_64B", || {
            black_box(a.as_slice() == b.as_slice());
        });
        let a = long_b.clone();
        let b = long_b.clone();
        bench("Vec<u8>", "eq_heap_64B", || {
            black_box(&a == &b);
        });
        let a = long_s.clone();
        let b = long_s.clone();
        bench("std::String", "eq_heap_64B", || {
            black_box(&a == &b);
        });
        let a: SmartString = long_s.as_str().into();
        let b: SmartString = long_s.as_str().into();
        bench("smartstring", "eq_heap_64B", || {
            black_box(&a == &b);
        });
        let a: CompactString = long_s.as_str().into();
        let b: CompactString = long_s.as_str().into();
        bench("compact_str", "eq_heap_64B", || {
            black_box(&a == &b);
        });
        let a: SmolStr = long_s.as_str().into();
        let b: SmolStr = long_s.as_str().into();
        bench("smol_str", "eq_heap_64B", || {
            black_box(&a == &b);
        });
    }

    // ---- from_bytes / from_str (12B) ----
    bench("kevy-bytes", "from_bytes_inline_12B", || {
        black_box(SmallBytes::from_slice(black_box(short_b)));
    });
    bench("Vec<u8>", "from_bytes_inline_12B", || {
        black_box(Vec::<u8>::from(black_box(short_b)));
    });
    bench("std::String", "from_str_inline_12B", || {
        black_box(std::string::String::from(black_box(short_s)));
    });
    bench("smartstring", "from_str_inline_12B", || {
        let s: SmartString = black_box(short_s).into();
        black_box(s);
    });
    bench("compact_str", "from_str_inline_12B", || {
        let s: CompactString = black_box(short_s).into();
        black_box(s);
    });
    bench("smol_str", "from_str_inline_12B", || {
        let s: SmolStr = black_box(short_s).into();
        black_box(s);
    });

    // ---- from_bytes / from_str (64B) ----
    bench("kevy-bytes", "from_bytes_heap_64B", || {
        black_box(SmallBytes::from_slice(black_box(&long_b)));
    });
    bench("Vec<u8>", "from_bytes_heap_64B", || {
        black_box(Vec::<u8>::from(black_box(long_b.as_slice())));
    });
    bench("std::String", "from_str_heap_64B", || {
        black_box(std::string::String::from(black_box(long_s.as_str())));
    });
    bench("smartstring", "from_str_heap_64B", || {
        let s: SmartString = black_box(long_s.as_str()).into();
        black_box(s);
    });
    bench("compact_str", "from_str_heap_64B", || {
        let s: CompactString = black_box(long_s.as_str()).into();
        black_box(s);
    });
    bench("smol_str", "from_str_heap_64B", || {
        let s: SmolStr = black_box(long_s.as_str()).into();
        black_box(s);
    });
}
