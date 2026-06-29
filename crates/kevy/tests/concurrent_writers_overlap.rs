//! Concurrent multi-writer chaos test — v1.35 (5/5 of the testing-
//! standards categories).
//!
//! N writer threads all SET the SAME set of shared keys, each writing
//! its own unique values. Each ACK'd write is logged with (writer_id,
//! key, value). After the run, for each shared key we read the stored
//! value and verify it's IN THE SET of values that some writer ACK'd
//! for that key. The stored value must NEVER be a value the server
//! fabricated — every value present must trace back to a real ACK'd
//! write.
//!
//! This catches:
//! - Cross-writer interference (writer A's value seen for writer B's key)
//! - Lost updates that change the value to something nobody wrote
//! - Cross-shard ordering bugs that mix values between commands
//! - Replication apply-order bugs (if replication is on)
//!
//! Then SIGKILL + restart and re-verify — the invariant must hold
//! after the AOF replay too.
//!
//! Strict asserts:
//! - For each shared key, the post-restart value is either:
//!   - Some ACK'd value for that key (the no-fabrication invariant), OR
//!   - Nil (the key was in the lost tail at SIGKILL)
//! - NEVER a value that no writer wrote.
//!
//! Gated `#[ignore]`. Run with:
//!
//! ```text
//! cargo build --release -p kevy
//! cargo test -p kevy --test concurrent_writers_overlap --release -- --ignored --nocapture
//! ```

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kevy_chaos::{Harness, HarnessConfig, KillSignal, pick_free_port};

const N_WRITERS: usize = 4;
const N_SHARED_KEYS: usize = 100;
const RUN_SECONDS: u64 = 3;

#[test]
#[ignore = "chaos test — opt-in via --ignored, needs `cargo build --release -p kevy` first"]
fn concurrent_writers_overlap_no_fabrication() {
    let bin_path = resolve_kevy_bin();
    let port = pick_free_port().expect("free port");
    let tmp = std::env::temp_dir().join(format!("kevy-chaos-overlap-{port}"));
    let _ = std::fs::remove_dir_all(&tmp);

    let cfg = HarnessConfig {
        kevy_bin: bin_path,
        ..HarnessConfig::new(tmp.clone(), port).with_fsync("everysec")
    };
    let mut h = Harness::spawn(cfg).expect("spawn kevy");

    // Per-key set of all values that were ACK'd by ANY writer.
    let acked: Arc<Mutex<HashMap<u32, HashSet<Vec<u8>>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let stop = Arc::new(AtomicBool::new(false));

    let mut handles = Vec::with_capacity(N_WRITERS);
    for writer_id in 0..N_WRITERS {
        let acked = Arc::clone(&acked);
        let stop = Arc::clone(&stop);
        handles.push(std::thread::spawn(move || {
            writer_loop(writer_id, port, acked, stop);
        }));
    }
    std::thread::sleep(Duration::from_secs(RUN_SECONDS));
    stop.store(true, Ordering::Relaxed);
    for handle in handles {
        let _ = handle.join();
    }
    let total_unique: usize = acked.lock().unwrap().values().map(HashSet::len).sum();
    let key_count = acked.lock().unwrap().len();
    eprintln!(
        "overlap: keys={key_count} total_acked_unique_values={total_unique} \
         (avg {} per key)",
        total_unique / key_count.max(1)
    );

    // Phase 1 verify: every key currently stored must be in its ACK set.
    let acked_snapshot = acked.lock().unwrap().clone();
    let pre_kill_violations = check_no_fabrication(port, &acked_snapshot);
    assert!(
        pre_kill_violations.is_empty(),
        "PRE-KILL FABRICATION DETECTED — kevy returned values nobody wrote:\n{}",
        pre_kill_violations
            .iter()
            .take(5)
            .map(|s| format!("  {s}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    eprintln!("overlap: pre-kill verify PASSED — no fabricated values across {key_count} keys");

    // Phase 2: SIGKILL + restart + re-verify.
    h.kill(KillSignal::Sigkill).expect("kill");
    h.restart().expect("restart");

    let post_kill_violations = check_no_fabrication(port, &acked_snapshot);
    assert!(
        post_kill_violations.is_empty(),
        "POST-KILL FABRICATION DETECTED — AOF replay produced values nobody wrote:\n{}",
        post_kill_violations
            .iter()
            .take(5)
            .map(|s| format!("  {s}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    eprintln!("overlap: post-restart verify PASSED — AOF replay preserved no-fabrication invariant");

    let _ = std::fs::remove_dir_all(&tmp);
}

fn writer_loop(
    writer_id: usize,
    port: u16,
    acked: Arc<Mutex<HashMap<u32, HashSet<Vec<u8>>>>>,
    stop: Arc<AtomicBool>,
) {
    let Ok(mut stream) = TcpStream::connect(format!("127.0.0.1:{port}")) else { return };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let mut seq: u64 = 0;
    let mut reply_buf = [0u8; 64];
    while !stop.load(Ordering::Relaxed) {
        let key_idx: u32 = (seq % N_SHARED_KEYS as u64) as u32;
        let key = format!("shared{key_idx:03}").into_bytes();
        let value = format!("w{writer_id}_s{seq}").into_bytes();
        let mut frame = Vec::with_capacity(64);
        frame.extend_from_slice(b"*3\r\n$3\r\nSET\r\n");
        frame.extend_from_slice(format!("${}\r\n", key.len()).as_bytes());
        frame.extend_from_slice(&key);
        frame.extend_from_slice(b"\r\n");
        frame.extend_from_slice(format!("${}\r\n", value.len()).as_bytes());
        frame.extend_from_slice(&value);
        frame.extend_from_slice(b"\r\n");
        if stream.write_all(&frame).is_err() {
            return;
        }
        match stream.read(&mut reply_buf) {
            Ok(n) if n >= 5 && reply_buf[..5] == *b"+OK\r\n" => {
                acked.lock().unwrap()
                    .entry(key_idx)
                    .or_default()
                    .insert(value);
                seq += 1;
            }
            _ => return,
        }
    }
}

/// For each key in `acked`, GET it from kevy and verify the value is
/// in the ACK set (or nil = lost). Returns descriptions of any
/// fabricated values (failures).
fn check_no_fabrication(
    port: u16,
    acked: &HashMap<u32, HashSet<Vec<u8>>>,
) -> Vec<String> {
    let Ok(mut s) = TcpStream::connect(format!("127.0.0.1:{port}")) else {
        return vec!["verify conn failed".into()];
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = s.set_write_timeout(Some(Duration::from_secs(30)));
    // Pipeline all GETs.
    let mut send_buf = Vec::with_capacity(acked.len() * 32);
    let mut keys_in_order: Vec<u32> = acked.keys().copied().collect();
    keys_in_order.sort();
    for &key_idx in &keys_in_order {
        let key = format!("shared{key_idx:03}").into_bytes();
        send_buf.extend_from_slice(b"*2\r\n$3\r\nGET\r\n");
        send_buf.extend_from_slice(format!("${}\r\n", key.len()).as_bytes());
        send_buf.extend_from_slice(&key);
        send_buf.extend_from_slice(b"\r\n");
    }
    let sh = std::thread::spawn(move || {
        s.write_all(&send_buf).expect("pipeline write");
        let _ = s.shutdown(std::net::Shutdown::Write);
        s
    });
    let mut s = sh.join().expect("send thread");
    let mut buf = Vec::with_capacity(64 * 1024);
    let mut tmp = vec![0u8; 64 * 1024];
    loop {
        match s.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(_) => break,
        }
    }

    let mut violations = Vec::new();
    let mut pos = 0usize;
    for &key_idx in &keys_in_order {
        match parse_one_reply(&buf, pos) {
            Some((Some(value), next)) => {
                let val_vec = value.to_vec();
                let known = acked.get(&key_idx);
                if !known.map(|s| s.contains(&val_vec)).unwrap_or(false) {
                    violations.push(format!(
                        "key=shared{key_idx:03} value={:?} not in ACK set",
                        String::from_utf8_lossy(&val_vec)
                    ));
                }
                pos = next;
            }
            Some((None, next)) => {
                // Nil is fine — the key was in the lost tail.
                pos = next;
            }
            None => {
                violations.push(format!("key=shared{key_idx:03} reply truncated"));
            }
        }
    }
    violations
}

fn parse_one_reply(buf: &[u8], start: usize) -> Option<(Option<&[u8]>, usize)> {
    let rest = &buf[start..];
    if rest.starts_with(b"$-1\r\n") {
        return Some((None, start + 5));
    }
    if rest.first() != Some(&b'$') {
        return None;
    }
    let nl = rest.iter().position(|&b| b == b'\n')?;
    let len_str = std::str::from_utf8(&rest[1..nl - 1]).ok()?;
    let len: usize = len_str.parse().ok()?;
    let body_start = nl + 1;
    let body_end = body_start + len;
    if rest.len() < body_end + 2 {
        return None;
    }
    Some((Some(&rest[body_start..body_end]), start + body_end + 2))
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
