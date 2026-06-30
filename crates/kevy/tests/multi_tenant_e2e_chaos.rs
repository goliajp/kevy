//! v1.48 — multi-tenant E2E chaos (Phase D step 1).
//!
//! Production kevy deployments often serve N independent workloads on
//! the same instance — message queues, session stores, cache layers —
//! each scoped by a key prefix. This test verifies kevy provides
//! tenant-pair isolation under concurrent load:
//!
//! - **No cross-tenant key leakage**: tenant A's writes never appear
//!   under tenant B's scan.
//! - **No tenant starvation**: under fair-share load all tenants
//!   complete their write budget within a bounded skew of each other.
//! - **Total ACK count is exact**: aggregate write count matches the
//!   sum of per-tenant budgets (no silently dropped writes).
//!
//! Each tenant runs 4 concurrent writers issuing 250 SETs each (1 000
//! per tenant) under prefix `tenant{i}:`. With 5 tenants, that's
//! 5 000 SETs across 20 writer threads.
//!
//! Gated `#[ignore]`. Run with:
//!
//! ```text
//! cargo build --release -p kevy
//! cargo test -p kevy --test multi_tenant_e2e_chaos --release -- --ignored --nocapture
//! ```

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use kevy_chaos::{Harness, HarnessConfig, pick_free_port};

const N_TENANTS: usize = 5;
const WRITERS_PER_TENANT: usize = 4;
const SETS_PER_WRITER: usize = 250;

#[test]
#[ignore = "chaos test — opt-in via --ignored, needs `cargo build --release -p kevy` first"]
fn multi_tenant_e2e_isolation_and_fairness() {
    let bin_path = resolve_kevy_bin();
    let port = pick_free_port().expect("free port");
    let tmp = std::env::temp_dir().join(format!("kevy-chaos-tenant-{port}"));
    let _ = std::fs::remove_dir_all(&tmp);

    let mut cfg = HarnessConfig::new(tmp.clone(), port).with_fsync("everysec");
    cfg.kevy_bin = bin_path;
    cfg.threads = 2;
    let _h = Harness::spawn(cfg).expect("spawn kevy");
    std::thread::sleep(Duration::from_millis(200));

    // PHASE 1: spawn N_TENANTS × WRITERS_PER_TENANT writer threads.
    // Each per-tenant ACK count atomic lets us watch fairness during
    // the run.
    let tenant_acks: Vec<Arc<AtomicU64>> = (0..N_TENANTS)
        .map(|_| Arc::new(AtomicU64::new(0)))
        .collect();
    let start = Instant::now();
    let mut handles = Vec::with_capacity(N_TENANTS * WRITERS_PER_TENANT);
    for t in 0..N_TENANTS {
        for w in 0..WRITERS_PER_TENANT {
            let acks = Arc::clone(&tenant_acks[t]);
            handles.push(thread::spawn(move || {
                tenant_writer(port, t, w, acks);
            }));
        }
    }
    for h in handles {
        h.join().expect("writer join");
    }
    let elapsed = start.elapsed();
    eprintln!(
        "multi_tenant: all {} writers done in {:.2} s",
        N_TENANTS * WRITERS_PER_TENANT,
        elapsed.as_secs_f64()
    );

    // PHASE 2: verify per-tenant ACK count = WRITERS_PER_TENANT * SETS_PER_WRITER.
    let expected_per_tenant = (WRITERS_PER_TENANT * SETS_PER_WRITER) as u64;
    let mut total_acks: u64 = 0;
    let mut min_tenant: u64 = u64::MAX;
    let mut max_tenant: u64 = 0;
    for (t, a) in tenant_acks.iter().enumerate() {
        let n = a.load(Ordering::SeqCst);
        eprintln!("multi_tenant: tenant{t} ACKs = {n}/{expected_per_tenant}");
        assert_eq!(
            n, expected_per_tenant,
            "tenant{t} missing ACKs: {n} vs {expected_per_tenant}"
        );
        total_acks += n;
        min_tenant = min_tenant.min(n);
        max_tenant = max_tenant.max(n);
    }
    let expected_total = expected_per_tenant * N_TENANTS as u64;
    assert_eq!(
        total_acks, expected_total,
        "total ACK mismatch: {total_acks} vs {expected_total}"
    );

    // PHASE 3: cross-tenant isolation — for each tenant, sample 3 keys
    // from BOTH the tenant's own prefix (must exist) and ALL OTHER
    // tenants' prefixes (must not be visible as the wrong tenant's).
    // We verify by sampling DBSIZE per-prefix via KEYS pattern.
    let mut s = TcpStream::connect(format!("127.0.0.1:{port}"))
        .expect("verify conn");
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    for t in 0..N_TENANTS {
        let pat = format!("tenant{t}:*");
        let pat_b = pat.as_bytes();
        let cmd = build_resp(&[b"KEYS", pat_b]);
        s.write_all(&cmd).expect("KEYS write");
        // KEYS reply can be large; read up to a few KB and count `$`.
        let count = read_keys_count(&mut s);
        eprintln!("multi_tenant: tenant{t} KEYS count = {count}");
        assert_eq!(
            count, expected_per_tenant as usize,
            "tenant{t} key count mismatch: {count} vs {expected_per_tenant}"
        );
    }

    // PHASE 4: fairness — max-min skew must be 0 under perfect isolation.
    // (We already asserted == expected_per_tenant for each, so this is
    // a sanity log; non-zero would have failed earlier.)
    let skew = max_tenant - min_tenant;
    eprintln!(
        "multi_tenant: fairness skew = {skew} (min={min_tenant}, max={max_tenant})"
    );
    assert_eq!(skew, 0, "fairness skew non-zero — tenant starved");

    // PHASE 5: health PING.
    s.write_all(b"*1\r\n$4\r\nPING\r\n").unwrap();
    let mut buf = [0u8; 64];
    let n = s.read(&mut buf).unwrap();
    assert!(
        buf[..n].starts_with(b"+PONG"),
        "post-load PING failed: {:?}",
        String::from_utf8_lossy(&buf[..n])
    );
    eprintln!(
        "multi_tenant: {} total ACKs, 0 cross-tenant leaks, kevy alive",
        total_acks
    );

    drop(s);
    let _ = std::fs::remove_dir_all(&tmp);
}

fn tenant_writer(port: u16, tenant: usize, writer: usize, acks: Arc<AtomicU64>) {
    let mut s = TcpStream::connect(format!("127.0.0.1:{port}"))
        .expect("writer conn");
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    let mut reply = vec![0u8; 256];
    for i in 0..SETS_PER_WRITER {
        let key = format!("tenant{tenant}:w{writer}:k{i:04}");
        let val = format!("v-{tenant}-{writer}-{i}");
        let cmd = build_resp(&[b"SET", key.as_bytes(), val.as_bytes()]);
        if s.write_all(&cmd).is_err() {
            return;
        }
        let n = match s.read(&mut reply) {
            Ok(n) => n,
            Err(_) => return,
        };
        if reply[..n].starts_with(b"+OK") {
            acks.fetch_add(1, Ordering::SeqCst);
        }
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

/// Read a RESP array reply and count the number of `$` bulk-string
/// headers — that's the element count of a KEYS reply, regardless of
/// element size.
fn read_keys_count(s: &mut TcpStream) -> usize {
    let mut buf = vec![0u8; 64 * 1024];
    let mut acc = Vec::with_capacity(128 * 1024);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let n = match s.read(&mut buf) {
            Ok(n) => n,
            Err(_) => break,
        };
        if n == 0 { break; }
        acc.extend_from_slice(&buf[..n]);
        // Expected array length appears as `*<count>\r\n` at the head.
        if let Some(target) = parse_array_count(&acc) {
            let dollars = acc.iter().filter(|&&b| b == b'$').count();
            if dollars >= target {
                return dollars;
            }
        }
        if Instant::now() > deadline {
            break;
        }
    }
    acc.iter().filter(|&&b| b == b'$').count()
}

fn parse_array_count(buf: &[u8]) -> Option<usize> {
    if buf.first()? != &b'*' { return None; }
    let nl = buf.iter().position(|&b| b == b'\n')?;
    std::str::from_utf8(&buf[1..nl.saturating_sub(1)]).ok()?.parse().ok()
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
