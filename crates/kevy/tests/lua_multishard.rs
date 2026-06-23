//! v1.27.1 — multi-shard EVAL/EVALSHA/SCRIPT verification.
//!
//! v1.27.0 had two routing bugs surfaced by real client testing:
//!   1. EVAL ran on the connection's shard, so a SET on shard X
//!      followed by an EVAL doing `redis.call('GET', KEYS[1])` on
//!      shard Y missed.
//!   2. SCRIPT cache was per-Bridge (per-shard); SCRIPT LOAD on
//!      shard X plus EVALSHA on shard Y returned -NOSCRIPT.
//!
//! v1.27.1 fixes both:
//!   - `KevyCommands::route` now classifies EVAL/EVALSHA with
//!     `numkeys ≥ 1` as `Route::Single(3)`, so the runtime sends
//!     the command to KEYS[1]'s shard.
//!   - SCRIPT cache moved to a process-global `Mutex<HashMap>` in
//!     `cmd_lua.rs`, shared by all shards.
//!
//! This test boots a real 4-shard kevy server in-process and runs
//! the same canonical Lua patterns that broke v1.27.0 against it.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// All tests in this file share the process-global SCRIPT cache
/// (the v1.27.1 design — that's exactly why a SCRIPT LOAD on one
/// shard reaches an EVALSHA on another). A `SCRIPT FLUSH` from one
/// test would therefore wipe scripts another test just loaded.
/// Serialize via this gate.
fn gate() -> std::sync::MutexGuard<'static, ()> {
    static G: OnceLock<Mutex<()>> = OnceLock::new();
    G.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let p = l.local_addr().unwrap().port();
    drop(l);
    p
}

struct Server {
    port: u16,
    dir: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start(nshards: usize) -> Server {
        let port = free_port();
        let dir = std::env::temp_dir().join(format!(
            "kevy-lua-multishard-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = kevy_config::Config::default();
        cfg.server.port = port;
        cfg.server.threads = nshards;
        // Cluster disabled — this test exercises the single-port
        // multi-shard mode, the default for `kevy --threads N`.
        kevy::config_init(Arc::new(cfg));
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let dir_thread = dir.clone();
        let handle = std::thread::spawn(move || {
            let rt = kevy_rt::Runtime::new([127, 0, 0, 1], port, nshards, kevy::KevyCommands)
                .with_data_dir(dir_thread);
            rt.run(stop_thread).unwrap();
        });
        for _ in 0..400 {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return Server {
                    port,
                    dir,
                    stop,
                    handle: Some(handle),
                };
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("kevy server didn't bind {port}");
    }

    fn req(&self, parts: &[&[u8]]) -> Vec<u8> {
        let mut s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut buf = Vec::new();
        buf.extend_from_slice(format!("*{}\r\n", parts.len()).as_bytes());
        for p in parts {
            buf.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
            buf.extend_from_slice(p);
            buf.extend_from_slice(b"\r\n");
        }
        s.write_all(&buf).unwrap();
        let mut reply = Vec::new();
        let mut chunk = [0u8; 4096];
        // Read until we have a complete RESP frame. For these tests,
        // a single read after the server writes is enough — but loop
        // a couple times to be robust against TCP fragmenting.
        for _ in 0..8 {
            match s.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    reply.extend_from_slice(&chunk[..n]);
                    if looks_complete(&reply) {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        reply
    }
}

fn looks_complete(reply: &[u8]) -> bool {
    // Cheap heuristic: a complete RESP simple type ends with \r\n.
    // For arrays / bulks, we expect at least one terminator. These
    // tests have small replies (< 1 KB) so this is reliable.
    reply.ends_with(b"\r\n")
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Nudge the runtime: open a connection so the poll loop wakes.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(h) = self.handle.take() {
            // run() blocks until stop is observed; tests should exit
            // promptly. If the join hangs, the test runner will
            // surface the leak as a timeout.
            let _ = h.join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn eval_writes_visible_across_shards() {
    let _g = gate();
    // 4 shards. Two keys chosen to hash to *different* shards via
    // the default `hash(key) % nshards` routing (no `{tag}`), so a
    // v1.27.0-style bug would surface: SET key1 on shard A; EVAL
    // doing GET key1 ran on whatever shard the connection landed
    // on, missing the value most of the time.
    let s = Server::start(4);
    // Pre-seed via SET and then read through an EVAL — repeat many
    // times so we get coverage across all 4 shards regardless of
    // which shard our throwaway connections happen to land on.
    for i in 0..50 {
        let key = format!("multi:shard:{i}");
        let val = format!("v-{i}");
        let r_set = s.req(&[b"SET", key.as_bytes(), val.as_bytes()]);
        assert_eq!(r_set, b"+OK\r\n", "SET failed for {key}");
        let script = b"return redis.call('GET', KEYS[1])";
        let r_eval = s.req(&[b"EVAL", script, b"1", key.as_bytes()]);
        let want = format!("${}\r\n{}\r\n", val.len(), val);
        assert_eq!(
            r_eval,
            want.as_bytes(),
            "EVAL GET wrong reply for {key}: {:?}",
            String::from_utf8_lossy(&r_eval)
        );
    }
}

#[test]
fn redlock_canonical_works_across_shards() {
    let _g = gate();
    let s = Server::start(4);
    let unlock_script = b"if redis.call('GET', KEYS[1]) == ARGV[1] then\n\
                          return redis.call('DEL', KEYS[1])\n\
                          else\n\
                          return 0\n\
                          end";
    for i in 0..30 {
        let key = format!("lock:order:{i}");
        let token = format!("tok-{i}");
        // Acquire by SET, then unlock by EVAL — this is the
        // canonical Redlock pattern. Under v1.27.0 multi-shard, the
        // unlock EVAL would land on the wrong shard and return 0
        // ("not my lock"), leaving the lock leaked.
        assert_eq!(
            s.req(&[b"SET", key.as_bytes(), token.as_bytes()]),
            b"+OK\r\n"
        );
        let r = s.req(&[b"EVAL", unlock_script, b"1", key.as_bytes(), token.as_bytes()]);
        assert_eq!(r, b":1\r\n", "redlock unlock returned wrong reply for {key}");
        assert_eq!(s.req(&[b"GET", key.as_bytes()]), b"$-1\r\n", "lock leaked");
    }
}

#[test]
fn script_load_then_evalsha_across_shards() {
    let _g = gate();
    let s = Server::start(4);
    // Load on whichever shard the SCRIPT LOAD lands on (LOAD is
    // Route::Local).
    let r_load = s.req(&[b"SCRIPT", b"LOAD", b"return 'cached-' .. KEYS[1]"]);
    assert!(r_load.starts_with(b"$40\r\n"), "LOAD got {:?}", String::from_utf8_lossy(&r_load));
    let sha = r_load[5..45].to_vec();
    // EVALSHA the same SHA1 with 30 different keys hashing across
    // all 4 shards. Under v1.27.0, anything not on the original
    // load-shard would NOSCRIPT.
    for i in 0..30 {
        let key = format!("k-{i}");
        let r = s.req(&[b"EVALSHA", &sha, b"1", key.as_bytes()]);
        let want = format!("$8\r\ncached-{}\r\n", &key[2..3]);
        // The expected payload is dynamic per key; compute it.
        let want_payload = format!("cached-{key}");
        let want_bulk = format!("${}\r\n{}\r\n", want_payload.len(), want_payload);
        assert_eq!(
            r,
            want_bulk.as_bytes(),
            "EVALSHA missing for {key}: {:?} (sanity want={want})",
            String::from_utf8_lossy(&r),
        );
    }
}

#[test]
fn script_flush_clears_global_cache() {
    let _g = gate();
    let s = Server::start(4);
    let r_load = s.req(&[b"SCRIPT", b"LOAD", b"return 42"]);
    let sha = r_load[5..45].to_vec();
    // EXISTS sees it.
    let r_exists = s.req(&[b"SCRIPT", b"EXISTS", &sha]);
    assert_eq!(r_exists, b"*1\r\n:1\r\n");
    // FLUSH wipes the global cache.
    assert_eq!(s.req(&[b"SCRIPT", b"FLUSH"]), b"+OK\r\n");
    let r_after = s.req(&[b"SCRIPT", b"EXISTS", &sha]);
    assert_eq!(r_after, b"*1\r\n:0\r\n");
}

#[test]
fn route_eval_to_key1_shard() {
    use kevy_resp::Argv;
    use kevy_rt::Commands;
    let mut a = Argv::default();
    a.push(b"EVAL");
    a.push(b"return 1");
    a.push(b"1");
    a.push(b"mykey");
    let r = kevy::KevyCommands.route(&a);
    let s = format!("{r:?}");
    assert!(s.contains("Single(3)"), "EVAL with numkeys=1 must Route::Single(3) (KEYS[1] at args[3]), got: {s}");
}

#[test]
fn route_eval_numkeys0_local() {
    use kevy_resp::Argv;
    use kevy_rt::Commands;
    let mut a = Argv::default();
    a.push(b"EVAL");
    a.push(b"return 1");
    a.push(b"0");
    let r = kevy::KevyCommands.route(&a);
    let s = format!("{r:?}");
    assert!(s.contains("Local"), "EVAL with numkeys=0 must Route::Local, got: {s}");
}

#[test]
fn route_script_local() {
    use kevy_resp::Argv;
    use kevy_rt::Commands;
    let mut a = Argv::default();
    a.push(b"SCRIPT");
    a.push(b"LOAD");
    a.push(b"return 1");
    let r = kevy::KevyCommands.route(&a);
    let s = format!("{r:?}");
    assert!(s.contains("Local"), "SCRIPT must Route::Local (global cache), got: {s}");
}
