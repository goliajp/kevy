//! End-to-end `CONFIG SET` / `CONFIG REWRITE` integration tests.
//!
//! Unit tests in `crates/kevy/src/ops/config.rs::tests::apply_hot_set_*`
//! cover validation and per-field parsing. This file covers the live
//! flow: SET → `config_global::replace` → per-shard tick re-application
//! → externally observable change (CONFIG GET reflects the new value,
//! REWRITE writes a round-trippable TOML file).
//!
//! Each test installs a fresh `Config` into `config_global` (via
//! `kevy::config_init`, which is idempotent — first test wins, the
//! rest snapshot-and-restore around their own SET calls so the suite
//! is order-independent).

use std::io::{Read, Write};
use std::sync::{Arc, Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, Ordering};

use kevy_config::{Config, EvictionPolicy};

/// `config_global` is a process-wide singleton; tests in this file
/// each mutate + reset it, so they must NOT interleave. Each test
/// holds this mutex for its entire body. (`cargo test` runs tests
/// within a binary in parallel by default; this serialises them.)
fn serial_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Install (idempotent) + reset `config_global` to a fresh `Config`.
/// Every test calls this under the serial lock so the global starts
/// in a known state regardless of test order.
fn install_fresh_config(seed: Config) {
    kevy::config_init(Arc::new(Config::default()));
    kevy::config_replace(Arc::new(seed)).expect("config_replace after init");
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
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
        "expected {:?}",
        String::from_utf8_lossy(expected)
    );
}

/// Read a CONFIG GET reply and return the value for `key` (or None if
/// the reply was empty). Assumes a `[k1, v1]` shape — i.e. caller asked
/// for a single non-glob key.
fn read_config_get_value(s: &mut std::net::TcpStream, expected_key: &str) -> Option<String> {
    // We don't know the exact wire bytes (port varies; key/value can be
    // arbitrary), so we do a small ad-hoc RESP read.
    let mut buf = [0u8; 4096];
    let n = s.read(&mut buf).unwrap();
    let text = String::from_utf8_lossy(&buf[..n]).into_owned();
    // Expect `*N\r\n$<klen>\r\n<k>\r\n$<vlen>\r\n<v>\r\n`.
    let mut lines = text.split("\r\n");
    let header = lines.next()?;
    assert!(header.starts_with('*'), "expected RESP array header, got {text:?}");
    let n_elems: usize = header[1..].parse().unwrap();
    if n_elems == 0 {
        return None;
    }
    let _klen = lines.next()?;
    let k = lines.next()?;
    let _vlen = lines.next()?;
    let v = lines.next()?;
    assert_eq!(k, expected_key, "unexpected key in CONFIG GET reply: {text:?}");
    Some(v.to_string())
}

/// Start a runtime + run `body`, then stop. Identical shape to the
/// helper in `persistence.rs` — duplicated here so this test file
/// doesn't depend on test-mod re-exports (which `cargo test` doesn't
/// share across integration-test binaries).
fn with_runtime(port: u16, dir: &std::path::Path, nshards: usize, body: impl FnOnce(u16)) {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_t = stop.clone();
    let dir = dir.to_path_buf();
    let handle = std::thread::spawn(move || {
        let rt = kevy_rt::Runtime::new([127, 0, 0, 1], port, nshards, kevy::KevyCommands)
            .with_data_dir(dir);
        rt.run(stop_t).unwrap();
    });
    let mut up = false;
    for _ in 0..200 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            up = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(up, "runtime did not start");
    body(port);
    stop.store(true, Ordering::Relaxed);
    let _ = handle.join();
}

#[test]
fn config_set_maxmemory_takes_effect_globally() {
    let _guard = serial_lock();
    install_fresh_config(Config::default());

    let dir = std::env::temp_dir().join(format!(
        "kevy-cfgset-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let port = free_port();

    with_runtime(port, &dir, 1, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();

        // Baseline: maxmemory starts at 0.
        c.write_all(&req(&[b"CONFIG", b"GET", b"maxmemory"])).unwrap();
        assert_eq!(read_config_get_value(&mut c, "maxmemory"), Some("0".into()));

        // SET to 2 GiB.
        c.write_all(&req(&[b"CONFIG", b"SET", b"maxmemory", b"2gb"])).unwrap();
        read_reply(&mut c, b"+OK\r\n");

        // GET reflects the new value immediately (handler reads the
        // post-swap config_global).
        c.write_all(&req(&[b"CONFIG", b"GET", b"maxmemory"])).unwrap();
        let v = read_config_get_value(&mut c, "maxmemory")
            .expect("maxmemory after SET should be present");
        assert_eq!(v, (2u64 * 1024 * 1024 * 1024).to_string());

        // SET to an evicting policy.
        c.write_all(&req(&[b"CONFIG", b"SET", b"maxmemory-policy", b"allkeys-lfu"])).unwrap();
        read_reply(&mut c, b"+OK\r\n");
        c.write_all(&req(&[b"CONFIG", b"GET", b"maxmemory-policy"])).unwrap();
        assert_eq!(
            read_config_get_value(&mut c, "maxmemory-policy"),
            Some("allkeys-lfu".into()),
        );

        // The Live config the shard sees on its next tick should match.
        // (We can't inspect Store::max_memory externally, but if the
        // SET goes through and CONFIG GET sees it, the per-tick
        // re-apply path in `on_shard_tick` runs the same `set_max_memory`
        // call. Verified separately via `apply_hot_set_*` unit tests.)
    });

    // Reset to default so later tests start clean.
    kevy::config_replace(Arc::new(Config::default())).expect("replace");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn config_set_appendfsync_round_trips_through_get() {
    let _guard = serial_lock();
    install_fresh_config(Config::default());
    let dir = std::env::temp_dir().join(format!(
        "kevy-cfgset-fsync-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let port = free_port();

    with_runtime(port, &dir, 1, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        c.write_all(&req(&[b"CONFIG", b"GET", b"appendfsync"])).unwrap();
        assert_eq!(
            read_config_get_value(&mut c, "appendfsync"),
            Some("everysec".into()),
        );

        c.write_all(&req(&[b"CONFIG", b"SET", b"appendfsync", b"always"])).unwrap();
        read_reply(&mut c, b"+OK\r\n");

        c.write_all(&req(&[b"CONFIG", b"GET", b"appendfsync"])).unwrap();
        assert_eq!(
            read_config_get_value(&mut c, "appendfsync"),
            Some("always".into()),
        );
    });

    let _ = kevy::config_replace(Arc::new(Config::default()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn config_set_read_only_bind_returns_restart_required_error() {
    let _guard = serial_lock();
    install_fresh_config(Config::default());
    let dir = std::env::temp_dir().join(format!(
        "kevy-cfgset-ro-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let port = free_port();

    with_runtime(port, &dir, 1, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        c.write_all(&req(&[b"CONFIG", b"SET", b"bind", b"0.0.0.0"])).unwrap();
        let mut buf = [0u8; 256];
        let n = c.read(&mut buf).unwrap();
        let reply = String::from_utf8_lossy(&buf[..n]);
        assert!(reply.starts_with("-ERR"), "got: {reply:?}");
        assert!(
            reply.contains("can't be changed at runtime"),
            "got: {reply:?}",
        );
    });

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn config_rewrite_writes_round_trippable_file_from_source_path() {
    let _guard = serial_lock();
    let dir = std::env::temp_dir().join(format!(
        "kevy-cfgrewrite-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let toml_path = dir.join("kevy.toml");
    // Plant a config with a non-default field so the rewrite has
    // something interesting to verify.
    let mut seed = Config::default();
    seed.memory.maxmemory = 512 * 1024 * 1024;
    seed.memory.maxmemory_policy = EvictionPolicy::AllKeysLru;
    seed.source_path = Some(toml_path.clone());
    std::fs::write(&toml_path, seed.to_toml_string()).unwrap();

    // Install fresh + then replace with the source-path seed — this
    // gives REWRITE a path to write back to.
    install_fresh_config(seed.clone());

    let port = free_port();

    with_runtime(port, &dir, 1, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        // First mutate via CONFIG SET so the in-memory config diverges
        // from the on-disk file — that's the case REWRITE has to
        // re-serialise (i.e. NOT just touch the file with its old
        // contents).
        c.write_all(&req(&[b"CONFIG", b"SET", b"maxmemory", b"1gb"])).unwrap();
        read_reply(&mut c, b"+OK\r\n");
        c.write_all(&req(&[b"CONFIG", b"SET", b"maxmemory-policy", b"volatile-ttl"])).unwrap();
        read_reply(&mut c, b"+OK\r\n");

        c.write_all(&req(&[b"CONFIG", b"REWRITE"])).unwrap();
        read_reply(&mut c, b"+OK\r\n");
    });

    // Disk file should now reflect the CONFIG-SET changes.
    let rewritten = std::fs::read_to_string(&toml_path).unwrap();
    assert!(
        rewritten.contains(&format!("maxmemory         = {}", 1024 * 1024 * 1024)),
        "REWRITE did not emit the post-SET maxmemory:\n{rewritten}",
    );
    assert!(
        rewritten.contains("maxmemory_policy  = \"volatile-ttl\""),
        "REWRITE did not emit the post-SET policy:\n{rewritten}",
    );
    // Reparseable.
    let reparsed = Config::from_toml_str(&rewritten, Some(&toml_path)).expect("reparse");
    assert_eq!(reparsed.memory.maxmemory, 1024 * 1024 * 1024);
    assert_eq!(reparsed.memory.maxmemory_policy, EvictionPolicy::VolatileTtl);

    let _ = kevy::config_replace(Arc::new(Config::default()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn config_rewrite_without_source_path_returns_error() {
    let _guard = serial_lock();
    // Default config has no source_path. Reply must be the canonical
    // "running without a config file" -ERR.
    install_fresh_config(Config::default());
    let dir = std::env::temp_dir().join(format!(
        "kevy-cfgrewrite-nosrc-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let port = free_port();

    with_runtime(port, &dir, 1, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        c.write_all(&req(&[b"CONFIG", b"REWRITE"])).unwrap();
        let mut buf = [0u8; 256];
        let n = c.read(&mut buf).unwrap();
        let reply = String::from_utf8_lossy(&buf[..n]);
        assert!(reply.starts_with("-ERR"), "got: {reply:?}");
        assert!(
            reply.contains("running without a config file"),
            "got: {reply:?}",
        );
    });

    let _ = std::fs::remove_dir_all(&dir);
}
