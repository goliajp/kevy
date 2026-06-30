//! v1.50 — long-running soak chaos (Phase D step 3).
//!
//! Memory leaks, file-descriptor leaks, lock starvation, and slow
//! background-thread regressions only surface after sustained load.
//! This test drives mixed workload for a configurable duration and
//! samples memory at 5-second intervals to detect monotonic growth.
//!
//! Default soak: 60 seconds (CI-friendly). Override via env:
//!
//! ```text
//! KEVY_SOAK_SECS=3600   # 1 hour
//! KEVY_SOAK_SECS=86400  # 24 hours (the v2 acceptance gate)
//! ```
//!
//! Strict asserts:
//! - Memory samples taken every 5 s.
//! - Linear-regression slope of (sample-index, used_memory) over the
//!   second half of the run must be ≤ 256 KiB / sample. Bursts in the
//!   first half are tolerated (initial keyspace fill); second-half
//!   growth is leak-suspicious.
//! - Zero parse / RESP errors throughout.
//! - Post-soak PING +PONG.
//!
//! Gated `#[ignore]`. Run with:
//!
//! ```text
//! cargo build --release -p kevy
//! cargo test -p kevy --test soak_long_running_chaos --release -- --ignored --nocapture
//! ```

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use kevy_chaos::{Harness, HarnessConfig, pick_free_port};

const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);
const PRODUCERS: usize = 4;
const SLOPE_CAP_BYTES_PER_SAMPLE: i64 = 256 * 1024;

struct Lcg(u64);
impl Lcg {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0
    }
    fn next_pct(&mut self) -> u8 {
        (self.next_u64() % 100) as u8
    }
}

#[test]
#[ignore = "chaos test — opt-in via --ignored, needs `cargo build --release -p kevy` first"]
fn soak_long_running_no_leak() {
    let bin_path = resolve_kevy_bin();
    let port = pick_free_port().expect("free port");
    let tmp = std::env::temp_dir().join(format!("kevy-chaos-soak-{port}"));
    let _ = std::fs::remove_dir_all(&tmp);

    let soak_secs: u64 = std::env::var("KEVY_SOAK_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    eprintln!("soak: running for {soak_secs} s (override via KEVY_SOAK_SECS)");

    let mut cfg = HarnessConfig::new(tmp.clone(), port).with_fsync("everysec");
    cfg.kevy_bin = bin_path;
    cfg.threads = 2;
    let _h = Harness::spawn(cfg).expect("spawn kevy");
    std::thread::sleep(Duration::from_millis(200));

    let stop = Arc::new(AtomicBool::new(false));
    let acks = Arc::new(AtomicU64::new(0));
    let errs = Arc::new(AtomicU64::new(0));

    let mut producers = Vec::with_capacity(PRODUCERS);
    for p in 0..PRODUCERS {
        let stop = Arc::clone(&stop);
        let acks = Arc::clone(&acks);
        let errs = Arc::clone(&errs);
        producers.push(thread::spawn(move || soak_producer(port, p, stop, acks, errs)));
    }

    // Sample memory at 5-second intervals.
    let mut samples: Vec<u64> = Vec::with_capacity((soak_secs / 5 + 1) as usize);
    let soak_start = Instant::now();
    let soak_end = soak_start + Duration::from_secs(soak_secs);
    while Instant::now() < soak_end {
        let m = read_used_memory(port);
        let elapsed = soak_start.elapsed().as_secs();
        let n = acks.load(Ordering::SeqCst);
        eprintln!("soak: t={elapsed:>4}s used_memory={m} ACKs={n}");
        samples.push(m);
        thread::sleep(SAMPLE_INTERVAL);
    }
    stop.store(true, Ordering::SeqCst);
    for h in producers {
        h.join().ok();
    }

    let total_acks = acks.load(Ordering::SeqCst);
    let total_errs = errs.load(Ordering::SeqCst);
    eprintln!(
        "soak: done — {total_acks} ACKs / {total_errs} errs over {soak_secs} s ({} ACK/s)",
        total_acks / soak_secs.max(1)
    );
    assert_eq!(total_errs, 0, "errors during soak: {total_errs}");
    assert!(total_acks > 0, "no ACKs accumulated — producers didn't run");

    // Slope test on second half — slope of (index, sample) using OLS.
    if samples.len() >= 4 {
        let half = samples.len() / 2;
        let second = &samples[half..];
        let n = second.len() as i64;
        let sum_x: i64 = (0..n).sum();
        let sum_y: i64 = second.iter().map(|&v| v as i64).sum();
        let sum_xy: i64 = second.iter().enumerate()
            .map(|(i, &v)| (i as i64) * (v as i64))
            .sum();
        let sum_xx: i64 = (0..n).map(|i| i * i).sum();
        let denom = n * sum_xx - sum_x * sum_x;
        let slope = if denom == 0 {
            0
        } else {
            (n * sum_xy - sum_x * sum_y) / denom
        };
        eprintln!(
            "soak: second-half memory slope = {slope} B/sample (cap = {} B/sample, samples = {:?})",
            SLOPE_CAP_BYTES_PER_SAMPLE, second
        );
        assert!(
            slope <= SLOPE_CAP_BYTES_PER_SAMPLE,
            "soak: memory leak suspected — second-half slope {slope} B/sample > cap {SLOPE_CAP_BYTES_PER_SAMPLE} B/sample"
        );
    } else {
        eprintln!("soak: only {} samples — slope test skipped (need >=4)", samples.len());
    }

    // Post-soak PING.
    let mut ping = TcpStream::connect(format!("127.0.0.1:{port}"))
        .expect("post-soak conn");
    let _ = ping.set_read_timeout(Some(Duration::from_secs(2)));
    ping.write_all(b"*1\r\n$4\r\nPING\r\n").unwrap();
    let mut buf = [0u8; 64];
    let n = ping.read(&mut buf).unwrap();
    assert!(
        buf[..n].starts_with(b"+PONG"),
        "post-soak PING failed: {:?}",
        String::from_utf8_lossy(&buf[..n])
    );
    eprintln!("soak: kevy alive after {soak_secs}s soak");

    drop(ping);
    let _ = std::fs::remove_dir_all(&tmp);
}

fn soak_producer(
    port: u16,
    producer_id: usize,
    stop: Arc<AtomicBool>,
    acks: Arc<AtomicU64>,
    errs: Arc<AtomicU64>,
) {
    let mut conn = match TcpStream::connect(format!("127.0.0.1:{port}")) {
        Ok(s) => s,
        Err(_) => return,
    };
    let _ = conn.set_read_timeout(Some(Duration::from_secs(5)));
    let mut rng = Lcg(0xBA53_BA11_BA51_C000u64.wrapping_add(producer_id as u64));
    let mut reply = vec![0u8; 16 * 1024];
    let mut iter: u64 = 0;
    while !stop.load(Ordering::Relaxed) {
        let pct = rng.next_pct();
        let key_idx = rng.next_u64() % 5_000;
        let cmd = if pct < 60 {
            // SET cycles same key set — exercises overwrite path.
            let key = format!("soak:p{producer_id}:s:{key_idx:05}");
            let val = format!("v-{iter:08}");
            build_resp(&[b"SET", key.as_bytes(), val.as_bytes()])
        } else if pct < 80 {
            // GET hot read.
            let key = format!("soak:p{producer_id}:s:{key_idx:05}");
            build_resp(&[b"GET", key.as_bytes()])
        } else if pct < 90 {
            // DEL — keeps keyspace bounded.
            let key = format!("soak:p{producer_id}:s:{key_idx:05}");
            build_resp(&[b"DEL", key.as_bytes()])
        } else {
            // HINCRBY — exercises a hash mutation path.
            let key = format!("soak:p{producer_id}:h:{key_idx:05}");
            build_resp(&[b"HINCRBY", key.as_bytes(), b"counter", b"1"])
        };
        if conn.write_all(&cmd).is_err() {
            errs.fetch_add(1, Ordering::SeqCst);
            return;
        }
        let n = match conn.read(&mut reply) {
            Ok(n) => n,
            Err(_) => {
                errs.fetch_add(1, Ordering::SeqCst);
                return;
            }
        };
        if n == 0 {
            errs.fetch_add(1, Ordering::SeqCst);
            return;
        }
        let leading = reply[0];
        if !matches!(leading, b'+' | b'-' | b'$' | b':' | b'*') {
            errs.fetch_add(1, Ordering::SeqCst);
        } else {
            acks.fetch_add(1, Ordering::SeqCst);
        }
        iter = iter.wrapping_add(1);
    }
}

fn build_resp(args: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(format!("*{}\r\n", args.len()).as_bytes());
    for a in args {
        out.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
        out.extend_from_slice(a);
        out.extend_from_slice(b"\r\n");
    }
    out
}

fn read_used_memory(port: u16) -> u64 {
    let mut s = match TcpStream::connect(format!("127.0.0.1:{port}")) {
        Ok(s) => s,
        Err(_) => return 0,
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    if s.write_all(b"*2\r\n$4\r\nINFO\r\n$6\r\nmemory\r\n").is_err() {
        return 0;
    }
    let mut buf = vec![0u8; 16 * 1024];
    let n = match s.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return 0,
    };
    let text = String::from_utf8_lossy(&buf[..n]);
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("used_memory:") {
            if let Ok(v) = rest.trim().parse::<u64>() {
                return v;
            }
        }
    }
    0
}

fn resolve_kevy_bin() -> PathBuf {
    if let Ok(p) = std::env::var("KEVY_BIN") {
        return PathBuf::from(p);
    }
    let here = std::env::current_dir().unwrap();
    let mut p = here.clone();
    loop {
        let candidate = p.join("target/release/kevy");
        if candidate.exists() {
            return candidate;
        }
        if !p.pop() {
            panic!(
                "kevy release binary not found above {}; run `cargo build --release -p kevy` first or set KEVY_BIN",
                here.display()
            );
        }
    }
}
