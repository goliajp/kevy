//! Keyspace notifications — Redis-compatible
//! `__keyspace@0__:<key>` + `__keyevent@0__:<event>` channels.
//!
//! Config: `notify_keyspace_events` is a string of flag chars
//! (K=keyspace channel, E=keyevent channel, g/$/l/s/h/z=event classes,
//! A=alias for all 6 classes). Default empty = OFF: writes pay one
//! bool-OR + skip, no publish.
//!
//! Hot-reload: `CONFIG SET notify_keyspace_events "KEA"` flips it on
//! at runtime via the existing live_runtime_config tick.

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

struct Server {
    port: u16,
    dir: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    /// Spin a server with `notify_keyspace_events = <flags>` pre-set
    /// in the config (so the first write already publishes — no
    /// CONFIG SET race window).
    ///
    /// The caller MUST be holding the START_GATE mutex for the
    /// duration of the test — config_global is process-singleton, so
    /// parallel tests would race each other's notify-flags installs.
    /// (We can't hold the gate inside this fn for the test's lifetime
    /// without leaking 'static.)
    fn start_with_flags(flags: &str, nshards: usize) -> Self {
        let port = free_port();
        let dir = std::env::temp_dir().join(format!(
            "kevy-keynotify-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        // Install a process-wide config with the flags set. config_global
        // is process-singleton — tests in this binary inherit each other's
        // installs. We init once + push the flag through CONFIG SET
        // after binding (via the live tick path).
        let mut cfg = kevy_config::Config::default();
        cfg.notification.notify_keyspace_events = flags.to_string();
        // config_global is process-singleton — `init` only takes once;
        // subsequent tests in this binary use `replace` to push fresh
        // flags. Either way, the runtime spawned below reads the live
        // value via `live_runtime_config` on each shard tick.
        // config_global::init is once-only — first test wins; the rest
        // use `replace` to push fresh flags. Either way, the runtime
        // spawned below reads the live value via `live_runtime_config`
        // on each shard tick.
        let arc = std::sync::Arc::new(cfg);
        kevy::config_init(arc.clone());
        let _ = kevy::config_replace(arc);

        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let dir_thread = dir.clone();
        let handle = std::thread::spawn(move || {
            let rt = kevy_rt::Runtime::new([127, 0, 0, 1], port, nshards, kevy::KevyCommands)
                .with_data_dir(dir_thread);
            rt.run(stop_thread).unwrap();
        });
        for _ in 0..200 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // Wait several ticks so each shard's apply_live_runtime_config
        // has definitely latched the flags. tick_interval defaults to
        // 100ms; first tick fires after that elapses, so 500ms = ≥3 ticks.
        std::thread::sleep(std::time::Duration::from_millis(500));
        Self { port, dir, stop, handle: Some(handle) }
    }

    fn connect(&self) -> std::net::TcpStream {
        let s = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        s
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn keyspace_channel_fires_on_set() {
    let _gate = START_GATE.lock().unwrap_or_else(|e| e.into_inner());
    // `K` = keyspace channel; `$` = string class. SET → fires
    // `__keyspace@0__:k` with payload `set`.
    let srv = Server::start_with_flags("K$", 1);

    let mut sub = srv.connect();
    sub.write_all(&req(&[b"SUBSCRIBE", b"__keyspace@0__:k"])).unwrap();
    read_reply(
        &mut sub,
        b"*3\r\n$9\r\nsubscribe\r\n$16\r\n__keyspace@0__:k\r\n:1\r\n",
    );

    let mut w = srv.connect();
    w.write_all(&req(&[b"SET", b"k", b"value"])).unwrap();
    read_reply(&mut w, b"+OK\r\n");

    read_reply(
        &mut sub,
        b"*3\r\n$7\r\nmessage\r\n$16\r\n__keyspace@0__:k\r\n$3\r\nset\r\n",
    );
}

#[test]
fn keyevent_channel_fires_on_set() {
    let _gate = START_GATE.lock().unwrap_or_else(|e| e.into_inner());
    // `E` = keyevent channel; `$` = string class. SET → fires
    // `__keyevent@0__:set` with payload = key name.
    let srv = Server::start_with_flags("E$", 1);

    let mut sub = srv.connect();
    sub.write_all(&req(&[b"SUBSCRIBE", b"__keyevent@0__:set"])).unwrap();
    read_reply(
        &mut sub,
        b"*3\r\n$9\r\nsubscribe\r\n$18\r\n__keyevent@0__:set\r\n:1\r\n",
    );

    let mut w = srv.connect();
    w.write_all(&req(&[b"SET", b"foo", b"bar"])).unwrap();
    read_reply(&mut w, b"+OK\r\n");

    read_reply(
        &mut sub,
        b"*3\r\n$7\r\nmessage\r\n$18\r\n__keyevent@0__:set\r\n$3\r\nfoo\r\n",
    );
}

#[test]
fn both_channels_with_alias_all_classes() {
    let _gate = START_GATE.lock().unwrap_or_else(|e| e.into_inner());
    // `KEA` = both channels + all 6 event classes.
    let srv = Server::start_with_flags("KEA", 1);

    let mut sub = srv.connect();
    sub.write_all(&req(&[b"PSUBSCRIBE", b"__key*@0__:*"])).unwrap();
    // Drain the psubscribe ack.
    let mut sink = vec![0u8; 256];
    let _ = sub.read(&mut sink).unwrap();

    let mut w = srv.connect();
    w.write_all(&req(&[b"HSET", b"h", b"f", b"v"])).unwrap();
    read_reply(&mut w, b":1\r\n");

    // Expect TWO pmessage frames: keyspace + keyevent. Read into a
    // buffer and assert both substrings (order non-deterministic
    // across the per-conn deliveries).
    let mut buf = vec![0u8; 256];
    let n = sub.read(&mut buf).unwrap();
    let s = &buf[..n];
    let keyspace_frame = b"__keyspace@0__:h";
    let keyevent_frame = b"__keyevent@0__:hset";
    assert!(
        s.windows(keyspace_frame.len()).any(|w| w == keyspace_frame),
        "missing keyspace pmessage in {:?}",
        String::from_utf8_lossy(s)
    );
    assert!(
        s.windows(keyevent_frame.len()).any(|w| w == keyevent_frame),
        "missing keyevent pmessage in {:?}",
        String::from_utf8_lossy(s)
    );
}

#[test]
fn default_off_emits_nothing() {
    let _gate = START_GATE.lock().unwrap_or_else(|e| e.into_inner());
    // No flags → no publish, no measurable difference between conns
    // that subscribe to the keyspace/event channels.
    let srv = Server::start_with_flags("", 1);

    let mut sub = srv.connect();
    sub.write_all(&req(&[b"PSUBSCRIBE", b"__key*@0__:*"])).unwrap();
    let mut sink = vec![0u8; 256];
    let _ = sub.read(&mut sink).unwrap();

    let mut w = srv.connect();
    w.write_all(&req(&[b"SET", b"any", b"val"])).unwrap();
    read_reply(&mut w, b"+OK\r\n");

    // sub should see nothing within a short read window.
    sub.set_read_timeout(Some(std::time::Duration::from_millis(200))).unwrap();
    let mut buf = [0u8; 64];
    let r = sub.read(&mut buf);
    match r {
        Err(e) => assert!(matches!(
            e.kind(),
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
        )),
        Ok(n) => panic!(
            "expected no pubsub traffic when notify is off; got {n} bytes: {:?}",
            String::from_utf8_lossy(&buf[..n])
        ),
    }
}

#[test]
fn class_gate_filters_unrelated_events() {
    let _gate = START_GATE.lock().unwrap_or_else(|e| e.into_inner());
    // `K$` enables ONLY string-class events. A HSET (hash class)
    // must NOT trigger any keyspace channel publish — even though
    // the `K` channel itself is on, the class bit is off.
    let srv = Server::start_with_flags("K$", 1);

    let mut sub = srv.connect();
    sub.write_all(&req(&[b"PSUBSCRIBE", b"__keyspace@0__:*"])).unwrap();
    let mut sink = vec![0u8; 256];
    let _ = sub.read(&mut sink).unwrap();

    let mut w = srv.connect();
    w.write_all(&req(&[b"HSET", b"hh", b"f", b"v"])).unwrap();
    read_reply(&mut w, b":1\r\n");

    // No keyspace pmessage for hash events when `h` flag is off.
    sub.set_read_timeout(Some(std::time::Duration::from_millis(200))).unwrap();
    let mut buf = [0u8; 64];
    assert!(
        sub.read(&mut buf).is_err(),
        "HSET should not fire a keyspace event when class `h` is disabled (flags=`K$`)"
    );

    // Then verify the gate still PASSES strings: SET DOES fire.
    sub.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
    w.write_all(&req(&[b"SET", b"str", b"v"])).unwrap();
    read_reply(&mut w, b"+OK\r\n");
    // Drain the pmessage (`*4\r\npmessage\r\n<pat>\r\n<chan>\r\n<payload>`).
    let mut buf = vec![0u8; 256];
    let n = sub.read(&mut buf).unwrap();
    assert!(
        buf[..n].windows(b"__keyspace@0__:str".len())
            .any(|w| w == b"__keyspace@0__:str"),
        "SET should fire a keyspace event when class `$` is enabled, got {:?}",
        String::from_utf8_lossy(&buf[..n])
    );
}
