//! SLOWLOG hot-reload — verifies `kevy_config::SlowlogSection` flows
//! through `KevyCommands::live_runtime_config` → `apply_live_runtime_config`
//! within one shard tick (≤ 100 ms by default, but we wait 500 ms for
//! safety).
//!
//! Lives in its own binary because `config_global::init` is process-
//! singleton — co-locating with the no-config slowlog tests would leak
//! state between files.

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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

fn read_exact_bytes(s: &mut std::net::TcpStream, expected: &[u8]) {
    let mut buf = vec![0u8; expected.len()];
    s.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, expected);
}

fn read_resp_line(s: &mut std::net::TcpStream) -> Vec<u8> {
    let mut out = Vec::new();
    let mut prev = 0u8;
    let mut byte = [0u8; 1];
    loop {
        s.read_exact(&mut byte).unwrap();
        out.push(byte[0]);
        if prev == b'\r' && byte[0] == b'\n' {
            return out;
        }
        prev = byte[0];
    }
}

fn slowlog_len(c: &mut std::net::TcpStream) -> i64 {
    c.write_all(&req(&[b"SLOWLOG", b"LEN"])).unwrap();
    let line = read_resp_line(c);
    std::str::from_utf8(&line[1..line.len() - 2])
        .unwrap()
        .parse()
        .unwrap()
}

#[test]
fn hot_reload_takes_effect_within_one_tick() {
    // Install config_global with slowlog OFF (slower_than = -1).
    let mut cfg = kevy_config::Config::default();
    cfg.slowlog.slower_than_micros = -1;
    cfg.slowlog.max_len = 128;
    kevy::config_init(Arc::new(cfg.clone()));
    let _ = kevy::config_replace(Arc::new(cfg));

    let port = free_port();
    let dir = std::env::temp_dir().join(format!(
        "kevy-slowlog-hot-{}",
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
        // Runtime builder gets a "default" 10_000-µs threshold, but the
        // live_runtime_config tick path overrides it on the first tick
        // with whatever config_global currently holds — we set it to
        // -1 above, so the runtime ends up OFF.
        let rt = kevy_rt::Runtime::new([127, 0, 0, 1], port, 1, kevy::KevyCommands)
            .with_data_dir(dir_thread)
            .with_aof(false);
        rt.run(stop_thread).unwrap();
    });
    for _ in 0..200 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    // Wait several ticks so `apply_live_runtime_config` has latched the
    // -1 setting from config_global on shard 0.
    std::thread::sleep(std::time::Duration::from_millis(500));

    let mut c = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    c.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
    // Phase 1: SLOWLOG is OFF → nothing recorded.
    for i in 0..5u32 {
        c.write_all(&req(&[b"SET", format!("p1-{i}").as_bytes(), b"v"]))
            .unwrap();
        read_exact_bytes(&mut c, b"+OK\r\n");
    }
    assert_eq!(slowlog_len(&mut c), 0, "phase 1: SLOWLOG must be off");

    // Phase 2: hot-swap the config to slower_than = 0 → record everything.
    let mut cfg2 = kevy_config::Config::default();
    cfg2.slowlog.slower_than_micros = 0;
    cfg2.slowlog.max_len = 128;
    kevy::config_replace(Arc::new(cfg2)).expect("replace");
    // Wait long enough for `apply_live_runtime_config` to pick up the
    // new threshold (≥ one tick interval = 100 ms; 500 ms is the same
    // safety margin keyspace_notify.rs uses).
    std::thread::sleep(std::time::Duration::from_millis(500));

    for i in 0..5u32 {
        c.write_all(&req(&[b"SET", format!("p2-{i}").as_bytes(), b"v"]))
            .unwrap();
        read_exact_bytes(&mut c, b"+OK\r\n");
    }
    let len = slowlog_len(&mut c);
    assert!(
        len >= 5,
        "phase 2: hot-reload should have enabled the ring, got {len}"
    );

    stop.store(true, Ordering::Relaxed);
    let _ = handle.join();
    let _ = std::fs::remove_dir_all(&dir);
}
