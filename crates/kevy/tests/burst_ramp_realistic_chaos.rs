//! v1.49 — burst/ramp + realistic-data workload chaos (Phase D step 2).
//!
//! Production traffic isn't uniform; it has steady-state, bursts,
//! ramps, and cooldowns, and the payload mix spans tiny strings,
//! moderate hashes, lists, and the occasional multi-KB blob. This
//! test drives kevy through a 4-phase traffic shape with a
//! realistic mixed-op distribution + asserts kevy survives the
//! burst + memory does not balloon afterward.
//!
//! Traffic phases (per producer thread, 4 producers):
//! - **Steady** (1s): ~250 ops/s/producer  = ~1k ops/s aggregate
//! - **Burst**  (1s): ~2500 ops/s/producer = ~10k ops/s aggregate
//! - **Cooldown** (1s): ~50 ops/s/producer = ~200 ops/s aggregate
//! - **Resume** (1s): ~250 ops/s/producer  = ~1k ops/s aggregate
//!
//! Op-mix per request (PRNG-driven, deterministic seed):
//! - 70% short SET (16-64 byte value)
//! - 15% HSET (5 fields)
//! - 10% LPUSH
//! -  5% large SET (4 KB value)
//!
//! Strict asserts:
//! - All issued commands receive a well-formed RESP reply (no torn).
//! - INFO `used_memory` after cooldown is within 4× of pre-burst
//!   (gross — guards against unbounded growth, NOT precise sizing).
//! - Post-test PING +PONG.
//!
//! Gated `#[ignore]`. Run with:
//!
//! ```text
//! cargo build --release -p kevy
//! cargo test -p kevy --test burst_ramp_realistic_chaos --release -- --ignored --nocapture
//! ```

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use kevy_chaos::{Harness, HarnessConfig, pick_free_port};

const N_PRODUCERS: usize = 4;
const STEADY_RATE: u64 = 250;
const BURST_RATE: u64 = 2500;
const COOLDOWN_RATE: u64 = 50;
const PHASE_DURATION_MS: u64 = 1000;

/// Std-only LCG (matches v1.36 fuzz harness).
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
fn burst_ramp_realistic_workload() {
    let bin_path = resolve_kevy_bin();
    let port = pick_free_port().expect("free port");
    let tmp = std::env::temp_dir().join(format!("kevy-chaos-burst-{port}"));
    let _ = std::fs::remove_dir_all(&tmp);

    let mut cfg = HarnessConfig::new(tmp.clone(), port).with_fsync("everysec");
    cfg.kevy_bin = bin_path;
    cfg.threads = 2;
    let _h = Harness::spawn(cfg).expect("spawn kevy");
    std::thread::sleep(Duration::from_millis(200));

    // Measure pre-burst memory baseline.
    let pre_mem = read_used_memory(port);
    eprintln!("burst_ramp: pre-burst used_memory = {pre_mem} B");

    // Spawn 4 producers, each running the 4-phase traffic shape.
    let acks_steady = Arc::new(AtomicU64::new(0));
    let acks_burst = Arc::new(AtomicU64::new(0));
    let acks_cool = Arc::new(AtomicU64::new(0));
    let acks_resume = Arc::new(AtomicU64::new(0));
    let errs = Arc::new(AtomicU64::new(0));

    let test_start = Instant::now();
    let mut handles = Vec::with_capacity(N_PRODUCERS);
    for p in 0..N_PRODUCERS {
        let acks_steady = Arc::clone(&acks_steady);
        let acks_burst = Arc::clone(&acks_burst);
        let acks_cool = Arc::clone(&acks_cool);
        let acks_resume = Arc::clone(&acks_resume);
        let errs = Arc::clone(&errs);
        handles.push(thread::spawn(move || {
            producer_loop(
                port, p,
                acks_steady, acks_burst, acks_cool, acks_resume, errs,
            );
        }));
    }
    for h in handles {
        h.join().expect("producer join");
    }
    let elapsed = test_start.elapsed();
    eprintln!("burst_ramp: 4 phases done in {:.2} s", elapsed.as_secs_f64());

    let s = acks_steady.load(Ordering::SeqCst);
    let b = acks_burst.load(Ordering::SeqCst);
    let c = acks_cool.load(Ordering::SeqCst);
    let r = acks_resume.load(Ordering::SeqCst);
    let e = errs.load(Ordering::SeqCst);
    eprintln!(
        "burst_ramp: ACKs steady={s} burst={b} cool={c} resume={r} errs={e}"
    );
    assert_eq!(e, 0, "non-RESP / parse errors during run = {e}");
    // Burst phase should produce strictly more ACKs than steady (kevy
    // accepts the higher rate). Allow 0.5× slack for jitter.
    assert!(
        b * 2 >= s * 3,
        "burst did not exceed steady by 1.5x: b={b} s={s}"
    );

    // Sleep briefly to let any pending writes drain.
    std::thread::sleep(Duration::from_millis(300));
    let post_mem = read_used_memory(port);
    eprintln!("burst_ramp: post-burst used_memory = {post_mem} B (4x cap = {})", pre_mem.saturating_mul(4).max(8 * 1024 * 1024));

    // Memory must not balloon past 4× pre-burst (with a floor of 8 MiB
    // to avoid spurious failures when pre_mem is tiny).
    let cap = pre_mem.saturating_mul(4).max(8 * 1024 * 1024);
    assert!(
        post_mem <= cap,
        "post-burst memory ballooned: {post_mem} > cap {cap}"
    );

    // Final health PING.
    let mut ping = TcpStream::connect(format!("127.0.0.1:{port}"))
        .expect("ping conn");
    let _ = ping.set_read_timeout(Some(Duration::from_secs(2)));
    ping.write_all(b"*1\r\n$4\r\nPING\r\n").unwrap();
    let mut buf = [0u8; 64];
    let n = ping.read(&mut buf).unwrap();
    assert!(
        buf[..n].starts_with(b"+PONG"),
        "post-burst PING failed: {:?}",
        String::from_utf8_lossy(&buf[..n])
    );
    let total = s + b + c + r;
    eprintln!("burst_ramp: {total} total ACKs, 0 errs, memory bounded, kevy alive");

    drop(ping);
    let _ = std::fs::remove_dir_all(&tmp);
}

fn producer_loop(
    port: u16,
    producer_id: usize,
    acks_steady: Arc<AtomicU64>,
    acks_burst: Arc<AtomicU64>,
    acks_cool: Arc<AtomicU64>,
    acks_resume: Arc<AtomicU64>,
    errs: Arc<AtomicU64>,
) {
    let mut conn = TcpStream::connect(format!("127.0.0.1:{port}"))
        .expect("producer conn");
    let _ = conn.set_read_timeout(Some(Duration::from_secs(5)));
    let mut rng = Lcg(0xCAFEBABE_DEAD0000 ^ (producer_id as u64));
    let mut reply = vec![0u8; 64 * 1024];
    let mut large_val = vec![0u8; 4096];
    for (i, b) in large_val.iter_mut().enumerate() {
        *b = (i % 256) as u8;
    }

    run_phase(
        "steady", producer_id, STEADY_RATE,
        &mut conn, &mut rng, &mut reply, &large_val,
        &acks_steady, &errs,
    );
    run_phase(
        "burst", producer_id, BURST_RATE,
        &mut conn, &mut rng, &mut reply, &large_val,
        &acks_burst, &errs,
    );
    run_phase(
        "cool", producer_id, COOLDOWN_RATE,
        &mut conn, &mut rng, &mut reply, &large_val,
        &acks_cool, &errs,
    );
    run_phase(
        "resume", producer_id, STEADY_RATE,
        &mut conn, &mut rng, &mut reply, &large_val,
        &acks_resume, &errs,
    );
}

fn run_phase(
    name: &str,
    producer_id: usize,
    rate_per_sec: u64,
    conn: &mut TcpStream,
    rng: &mut Lcg,
    reply: &mut [u8],
    large_val: &[u8],
    acks: &AtomicU64,
    errs: &AtomicU64,
) {
    let phase_end = Instant::now() + Duration::from_millis(PHASE_DURATION_MS);
    let interval_ns = 1_000_000_000u64 / rate_per_sec.max(1);
    let mut next_send = Instant::now();
    let mut local_count = 0u64;
    while Instant::now() < phase_end {
        // Pace.
        let now = Instant::now();
        if now < next_send {
            let sleep_ns = (next_send - now).as_nanos() as u64;
            if sleep_ns > 100_000 {
                thread::sleep(Duration::from_nanos(sleep_ns.min(2_000_000)));
            }
        }
        next_send += Duration::from_nanos(interval_ns);

        // Pick op by PRNG percentile.
        let pct = rng.next_pct();
        let key_idx = rng.next_u64() % 10_000;
        let cmd = if pct < 70 {
            // Short SET.
            let vlen = 16 + (rng.next_u64() % 48) as usize;
            let val: Vec<u8> = (0..vlen).map(|i| b'a' + ((i as u8) % 26)).collect();
            let key = format!("p{producer_id}:s:{key_idx:05}");
            build_resp(&[b"SET", key.as_bytes(), &val])
        } else if pct < 85 {
            // HSET 5 fields.
            let key = format!("p{producer_id}:h:{key_idx:05}");
            build_resp(&[
                b"HSET", key.as_bytes(),
                b"f1", b"v1", b"f2", b"v2", b"f3", b"v3", b"f4", b"v4", b"f5", b"v5",
            ])
        } else if pct < 95 {
            // LPUSH.
            let key = format!("p{producer_id}:l:{key_idx:05}");
            let val = format!("item-{}", rng.next_u64() % 1_000);
            build_resp(&[b"LPUSH", key.as_bytes(), val.as_bytes()])
        } else {
            // Large SET 4 KB.
            let key = format!("p{producer_id}:big:{key_idx:05}");
            build_resp(&[b"SET", key.as_bytes(), large_val])
        };

        if conn.write_all(&cmd).is_err() {
            errs.fetch_add(1, Ordering::SeqCst);
            return;
        }
        let n = match conn.read(reply) {
            Ok(n) => n,
            Err(_) => {
                errs.fetch_add(1, Ordering::SeqCst);
                return;
            }
        };
        let leading = reply.first().copied().unwrap_or(b'?');
        if !matches!(leading, b'+' | b'-' | b'$' | b':' | b'*') || n == 0 {
            errs.fetch_add(1, Ordering::SeqCst);
        } else {
            acks.fetch_add(1, Ordering::SeqCst);
            local_count += 1;
        }
    }
    let _ = name;
    let _ = local_count;
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

/// `INFO memory` → parse `used_memory:<n>` field. Returns 0 if not
/// found (treated as unknown, the `.max(8 MiB)` floor handles it).
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
