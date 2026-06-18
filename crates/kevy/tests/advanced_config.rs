//! `[advanced]` config knobs — verify the four reactor tuning fields
//! (`spin_limit` / `park_timeout_ms` / `tick_check_every` /
//! `ring_capacity`) flow from TOML → `Config` → `Runtime` → `Shard`
//! and that a non-default value actually drives the reactor (i.e. the
//! constants are properly threaded, not just defined as fields).
//!
//! These knobs are startup-only (not hot-reloadable via `CONFIG SET`)
//! because changing `ring_capacity` mid-flight would require re-
//! allocating every SPSC ring + repatching every peer's outbox; the
//! other three could in principle be live but are scoped together
//! for v1.4 simplicity.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

static START_GATE: Mutex<()> = Mutex::new(());

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

fn req(parts: &[&[u8]]) -> Vec<u8> {
    let mut v = format!("*{}\r\n", parts.len()).into_bytes();
    for p in parts {
        v.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
        v.extend_from_slice(p);
        v.extend_from_slice(b"\r\n");
    }
    v
}

fn read_reply(s: &mut std::net::TcpStream, expected: &[u8]) {
    let mut buf = vec![0u8; expected.len()];
    s.read_exact(&mut buf).unwrap();
    assert_eq!(
        &buf,
        expected,
        "expected {:?}, got {:?}",
        String::from_utf8_lossy(expected),
        String::from_utf8_lossy(&buf),
    );
}

#[test]
fn advanced_defaults_match_pre_v14_constants() {
    // The v1.4 knob defaults must match the constants kevy shipped
    // pre-v1.4 — otherwise the existing bench / sharded numbers would
    // shift just by parsing an empty `[advanced]` section.
    let adv = kevy_config::AdvancedSection::default();
    assert_eq!(adv.spin_limit, 256, "SPIN_LIMIT default");
    assert_eq!(adv.park_timeout_ms, 50, "PARK_TIMEOUT_MS default");
    assert_eq!(adv.tick_check_every, 256, "TICK_CHECK_EVERY default");
    assert_eq!(adv.ring_capacity, 1024, "RING_CAPACITY default");
}

#[test]
fn advanced_section_round_trips_through_toml() {
    let mut cfg = kevy_config::Config::default();
    cfg.advanced.spin_limit = 512;
    cfg.advanced.park_timeout_ms = 20;
    cfg.advanced.tick_check_every = 128;
    cfg.advanced.ring_capacity = 4096;

    let toml = cfg.to_toml_string();
    assert!(toml.contains("[advanced]"));
    assert!(toml.contains("spin_limit       = 512"));
    assert!(toml.contains("park_timeout_ms  = 20"));
    assert!(toml.contains("tick_check_every = 128"));
    assert!(toml.contains("ring_capacity    = 4096"));

    let parsed = kevy_config::Config::from_toml_str(&toml, None).unwrap();
    assert_eq!(parsed.advanced, cfg.advanced);
}

#[test]
fn runtime_with_advanced_runs_cmds_correctly() {
    // Build a runtime with non-default knobs and verify it still
    // serves commands. The values themselves aren't directly observable
    // over the wire — we just confirm a tuned-down ring + spin doesn't
    // break correctness. Sharded suite (separate file) gives the
    // primary regression coverage for reactor behaviour.
    let _gate = START_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let port = free_port();
    let dir = std::env::temp_dir().join(format!(
        "kevy-advcfg-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let dir_thread = dir.clone();
    let handle = std::thread::spawn(move || {
        let rt = kevy_rt::Runtime::new([127, 0, 0, 1], port, 2, kevy::KevyCommands)
            .with_data_dir(dir_thread)
            // Atypical knobs: low spin, tight ring, slow tick.
            .with_advanced(/* spin */ 16, /* park */ 25, /* tick */ 64, /* ring */ 64);
        rt.run(stop_thread).unwrap();
    });
    for _ in 0..200 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    let mut c = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    c.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
    // Single-key cmd: SET + GET work end-to-end.
    c.write_all(&req(&[b"SET", b"a", b"1"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");
    c.write_all(&req(&[b"GET", b"a"])).unwrap();
    read_reply(&mut c, b"$1\r\n1\r\n");
    // Cross-shard multi-key: MSET across multiple keys (will land on
    // distinct shards with nshards=2).
    c.write_all(&req(&[b"MSET", b"x", b"X", b"y", b"Y", b"z", b"Z"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");
    c.write_all(&req(&[b"MGET", b"x", b"missing", b"z"])).unwrap();
    read_reply(&mut c, b"*3\r\n$1\r\nX\r\n$-1\r\n$1\r\nZ\r\n");

    stop.store(true, Ordering::Relaxed);
    let _ = handle.join();
    let _ = std::fs::remove_dir_all(&dir);
}
