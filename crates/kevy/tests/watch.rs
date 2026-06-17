//! `WATCH` / `UNWATCH` + `EXEC` pre-check fan-out semantics.
//!
//! `kevy-rt`'s atomic-CAS guarantees rest on:
//!  - same-shard: `bump_if_watched` on write + `key_version` on EXEC's
//!    pre-check, both single-threaded on the owning reactor → strict CAS;
//!  - cross-shard: each owning shard checks its own keys, the OR is
//!    folded on the origin shard → best-effort (the only race is the
//!    µs-scale window between the last `CheckWatch` reply and the queued
//!    cmds running, the same window Redis cluster mode has).
//!
//! These tests exercise both: same-shard via `nshards=1`, cross-shard via
//! `nshards=4` with keys spread across owning shards.

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
    fn start(nshards: usize) -> Server {
        let _gate = START_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let port = free_port();
        let dir = std::env::temp_dir().join(format!(
            "kevy-watch-{}",
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
            let rt = kevy_rt::Runtime::new([127, 0, 0, 1], port, nshards, kevy::KevyCommands)
                .with_data_dir(dir_thread);
            rt.run(stop_thread).unwrap();
        });
        let mut ready = false;
        for _ in 0..200 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                ready = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(ready, "runtime did not come up");
        Server {
            port,
            dir,
            stop,
            handle: Some(handle),
        }
    }

    fn connect(&self) -> std::net::TcpStream {
        let s = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(10)))
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

// ---------- single-shard: strict CAS ----------

#[test]
fn watch_then_exec_clean_commits() {
    let srv = Server::start(1);
    let mut c = srv.connect();

    c.write_all(&req(&[b"WATCH", b"k"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");
    c.write_all(&req(&[b"MULTI"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");
    c.write_all(&req(&[b"SET", b"k", b"v"])).unwrap();
    read_reply(&mut c, b"+QUEUED\r\n");
    c.write_all(&req(&[b"EXEC"])).unwrap();
    read_reply(&mut c, b"*1\r\n+OK\r\n");

    c.write_all(&req(&[b"GET", b"k"])).unwrap();
    read_reply(&mut c, b"$1\r\nv\r\n");
}

#[test]
fn watch_then_concurrent_write_aborts() {
    // Same shard (nshards=1) → strict CAS. Another conn writing the
    // watched key after WATCH but before EXEC must abort the EXEC with
    // a nil multi-bulk (`*-1\r\n`).
    let srv = Server::start(1);
    let mut c = srv.connect();
    let mut other = srv.connect();

    c.write_all(&req(&[b"SET", b"k", b"orig"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");

    c.write_all(&req(&[b"WATCH", b"k"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");

    // Another conn mutates k between WATCH and EXEC.
    other.write_all(&req(&[b"SET", b"k", b"stomp"])).unwrap();
    read_reply(&mut other, b"+OK\r\n");

    c.write_all(&req(&[b"MULTI"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");
    c.write_all(&req(&[b"SET", b"k", b"v"])).unwrap();
    read_reply(&mut c, b"+QUEUED\r\n");
    c.write_all(&req(&[b"EXEC"])).unwrap();
    read_reply(&mut c, b"*-1\r\n");

    // Queued SET never ran — k still has the stomp value.
    c.write_all(&req(&[b"GET", b"k"])).unwrap();
    read_reply(&mut c, b"$5\r\nstomp\r\n");
}

#[test]
fn unwatch_clears_watched_set() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    let mut other = srv.connect();

    c.write_all(&req(&[b"WATCH", b"k"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");
    c.write_all(&req(&[b"UNWATCH"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");

    // Stomp on what *would* have been watched — UNWATCH cleared the set,
    // so EXEC must commit normally.
    other.write_all(&req(&[b"SET", b"k", b"stomp"])).unwrap();
    read_reply(&mut other, b"+OK\r\n");

    c.write_all(&req(&[b"MULTI"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");
    c.write_all(&req(&[b"SET", b"k", b"v"])).unwrap();
    read_reply(&mut c, b"+QUEUED\r\n");
    c.write_all(&req(&[b"EXEC"])).unwrap();
    read_reply(&mut c, b"*1\r\n+OK\r\n");

    c.write_all(&req(&[b"GET", b"k"])).unwrap();
    read_reply(&mut c, b"$1\r\nv\r\n");
}

#[test]
fn watch_inside_multi_is_an_error() {
    let srv = Server::start(1);
    let mut c = srv.connect();

    c.write_all(&req(&[b"MULTI"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");
    c.write_all(&req(&[b"WATCH", b"k"])).unwrap();
    // WATCH inside MULTI returns an error (Redis semantics).
    let mut buf = [0u8; 64];
    let n = c.read(&mut buf).unwrap();
    assert!(
        buf[..n].starts_with(b"-ERR WATCH inside MULTI"),
        "got {:?}",
        String::from_utf8_lossy(&buf[..n])
    );
    // The MULTI is still open — DISCARD it cleanly so the conn stays usable.
    c.write_all(&req(&[b"DISCARD"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");
}

#[test]
fn discard_clears_watched_set() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    let mut other = srv.connect();

    c.write_all(&req(&[b"WATCH", b"k"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");
    c.write_all(&req(&[b"MULTI"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");
    c.write_all(&req(&[b"DISCARD"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");

    // DISCARD also unwatches — k stomp + new EXEC must commit.
    other.write_all(&req(&[b"SET", b"k", b"stomp"])).unwrap();
    read_reply(&mut other, b"+OK\r\n");

    c.write_all(&req(&[b"MULTI"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");
    c.write_all(&req(&[b"SET", b"k", b"v"])).unwrap();
    read_reply(&mut c, b"+QUEUED\r\n");
    c.write_all(&req(&[b"EXEC"])).unwrap();
    read_reply(&mut c, b"*1\r\n+OK\r\n");
}

#[test]
fn watch_no_stomp_then_exec_commits() {
    // WATCH a never-written key → version 0 stays 0 → CheckWatch sees
    // 0 == 0 → clean → commit. This is the "WATCH a fresh key" base
    // case that record_watch + key_version both default to 0 for.
    let srv = Server::start(1);
    let mut c = srv.connect();

    c.write_all(&req(&[b"WATCH", b"fresh"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");
    c.write_all(&req(&[b"MULTI"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");
    c.write_all(&req(&[b"SET", b"fresh", b"x"])).unwrap();
    read_reply(&mut c, b"+QUEUED\r\n");
    c.write_all(&req(&[b"EXEC"])).unwrap();
    read_reply(&mut c, b"*1\r\n+OK\r\n");
}

// ---------- cross-shard: best-effort CAS ----------

#[test]
fn watch_many_keys_cross_shard_clean_commits() {
    // 4 shards, WATCH ~24 keys — guaranteed to hit every shard (each key's
    // shard is `hash(key) % 4`). No other conn touches them; EXEC commits.
    let srv = Server::start(4);
    let mut c = srv.connect();

    let mut watch_req = vec![b"WATCH".as_slice()];
    let keys: Vec<String> = (0..24).map(|i| format!("xs:{i}")).collect();
    for k in &keys {
        watch_req.push(k.as_bytes());
    }
    c.write_all(&req(&watch_req)).unwrap();
    read_reply(&mut c, b"+OK\r\n");

    c.write_all(&req(&[b"MULTI"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");
    c.write_all(&req(&[b"SET", b"unrelated", b"v"])).unwrap();
    read_reply(&mut c, b"+QUEUED\r\n");
    c.write_all(&req(&[b"EXEC"])).unwrap();
    read_reply(&mut c, b"*1\r\n+OK\r\n");
}

#[test]
fn watch_many_keys_cross_shard_stomp_aborts() {
    // 4 shards, WATCH ~24 keys spread across all shards, another conn
    // mutates ONE of them — EXEC aborts even if the mutated key lives on
    // a different shard than the EXEC's queued writes.
    let srv = Server::start(4);
    let mut c = srv.connect();
    let mut other = srv.connect();

    let mut watch_req = vec![b"WATCH".as_slice()];
    let keys: Vec<String> = (0..24).map(|i| format!("xs:{i}")).collect();
    for k in &keys {
        watch_req.push(k.as_bytes());
    }
    c.write_all(&req(&watch_req)).unwrap();
    read_reply(&mut c, b"+OK\r\n");

    // Stomp on the middle key — fans out CheckWatch across all 4 shards.
    other.write_all(&req(&[b"SET", b"xs:12", b"stomp"])).unwrap();
    read_reply(&mut other, b"+OK\r\n");

    c.write_all(&req(&[b"MULTI"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");
    c.write_all(&req(&[b"SET", b"unrelated", b"v"])).unwrap();
    read_reply(&mut c, b"+QUEUED\r\n");
    c.write_all(&req(&[b"EXEC"])).unwrap();
    read_reply(&mut c, b"*-1\r\n");
}

#[test]
fn exec_after_watch_with_multi_cmd_queue() {
    // Several queued cmds + WATCH — clean path emits *N then each reply
    // in seq order. This exercises the placeholder-slot dispatch chain
    // for both Local (PING) and Single-key (SET/INCR/GET) routes.
    let srv = Server::start(4);
    let mut c = srv.connect();

    c.write_all(&req(&[b"WATCH", b"q:a"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");
    c.write_all(&req(&[b"MULTI"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");
    c.write_all(&req(&[b"SET", b"q:a", b"0"])).unwrap();
    read_reply(&mut c, b"+QUEUED\r\n");
    c.write_all(&req(&[b"INCR", b"q:a"])).unwrap();
    read_reply(&mut c, b"+QUEUED\r\n");
    c.write_all(&req(&[b"PING"])).unwrap();
    read_reply(&mut c, b"+QUEUED\r\n");
    c.write_all(&req(&[b"GET", b"q:a"])).unwrap();
    read_reply(&mut c, b"+QUEUED\r\n");
    c.write_all(&req(&[b"EXEC"])).unwrap();
    read_reply(
        &mut c,
        b"*4\r\n+OK\r\n:1\r\n+PONG\r\n$1\r\n1\r\n",
    );
}

// ---------- at_seq invariants: queued cmds inside WATCH'd EXEC ----------

#[test]
fn watched_exec_with_queued_multi_target_del() {
    // Queue a multi-key DEL inside a WATCH'd MULTI block. The
    // pre-allocated placeholder slot must be reconfigured to fan-out
    // shape (remaining = group count, agg = SumInt) by
    // `start_multi_at_seq`; otherwise the slot would stall the conn.
    let srv = Server::start(4);
    let mut c = srv.connect();

    // Seed 3 distinct keys (likely on different shards).
    for k in ["mk:a", "mk:b", "mk:c"] {
        c.write_all(&req(&[b"SET", k.as_bytes(), b"v"])).unwrap();
        read_reply(&mut c, b"+OK\r\n");
    }

    c.write_all(&req(&[b"WATCH", b"sentinel"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");
    c.write_all(&req(&[b"MULTI"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");
    c.write_all(&req(&[b"DEL", b"mk:a", b"mk:b", b"mk:c"])).unwrap();
    read_reply(&mut c, b"+QUEUED\r\n");
    c.write_all(&req(&[b"EXISTS", b"mk:a", b"mk:b", b"mk:c"])).unwrap();
    read_reply(&mut c, b"+QUEUED\r\n");
    c.write_all(&req(&[b"EXEC"])).unwrap();
    // *2 + :3 (DEL removed 3) + :0 (none EXIST after DEL)
    read_reply(&mut c, b"*2\r\n:3\r\n:0\r\n");
}

#[test]
fn watched_exec_with_queued_publish_emits_error_placeholder() {
    // PUBLISH inside MULTI is queued (TxnKind::Other). Inside a WATCH'd
    // EXEC its `start_command_at_seq` arm fills the placeholder with
    // an explicit "pub/sub inside MULTI" error — keeps the conn's seq
    // ring consistent (no stalled placeholder).
    let srv = Server::start(4);
    let mut c = srv.connect();

    c.write_all(&req(&[b"WATCH", b"pw:k"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");
    c.write_all(&req(&[b"MULTI"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");
    c.write_all(&req(&[b"PUBLISH", b"ch", b"x"])).unwrap();
    read_reply(&mut c, b"+QUEUED\r\n");
    c.write_all(&req(&[b"INCR", b"pw:k"])).unwrap();
    read_reply(&mut c, b"+QUEUED\r\n");
    c.write_all(&req(&[b"EXEC"])).unwrap();
    // *2 + error frame for PUBLISH + :1 for INCR.
    // Error frame: kevy v2-3a expanded the in-MULTI deny list to also
    // cover HELLO and RENAME (their orchestration doesn't have an
    // at_seq variant yet — see exec_watch.rs).
    let want = b"*2\r\n-ERR pub/sub or WATCH or HELLO or RENAME not allowed inside MULTI in v2-3a (queued-RENAME orchestration pending v2-3b)\r\n:1\r\n";
    read_reply(&mut c, want);
}

#[test]
fn watched_exec_with_queued_unwatch_is_ok_noop() {
    // UNWATCH queued inside MULTI is a no-op (the EXEC entry already
    // drained `watched`). The at_seq arm fills the placeholder with
    // `+OK\r\n` — verifying the queued UNWATCH doesn't desync the seq
    // ring or skip the +OK reply.
    let srv = Server::start(4);
    let mut c = srv.connect();

    c.write_all(&req(&[b"WATCH", b"u:k"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");
    c.write_all(&req(&[b"MULTI"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");
    c.write_all(&req(&[b"UNWATCH"])).unwrap();
    read_reply(&mut c, b"+QUEUED\r\n");
    c.write_all(&req(&[b"INCR", b"u:k"])).unwrap();
    read_reply(&mut c, b"+QUEUED\r\n");
    c.write_all(&req(&[b"EXEC"])).unwrap();
    // *2 + +OK (UNWATCH no-op) + :1 (INCR fresh key).
    read_reply(&mut c, b"*2\r\n+OK\r\n:1\r\n");
}

#[test]
fn watched_exec_quit_inside_multi_closes_after_drain() {
    // QUIT (`is_quit=true`) queued inside WATCH'd EXEC sets
    // `conn.closing=true` via `start_single_at_seq`'s is_quit branch
    // — covers the previously-untested `if is_quit { c.closing = true }`
    // path inside the at_seq variants. The conn closes after the
    // pre-QUIT replies are drained.
    let srv = Server::start(4);
    let mut c = srv.connect();

    c.write_all(&req(&[b"WATCH", b"q:k"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");
    c.write_all(&req(&[b"MULTI"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");
    c.write_all(&req(&[b"SET", b"q:k", b"v"])).unwrap();
    read_reply(&mut c, b"+QUEUED\r\n");
    c.write_all(&req(&[b"QUIT"])).unwrap();
    read_reply(&mut c, b"+QUEUED\r\n");
    c.write_all(&req(&[b"EXEC"])).unwrap();
    // *2 + +OK (SET) + +OK (QUIT reply).
    read_reply(&mut c, b"*2\r\n+OK\r\n+OK\r\n");
    // The conn now closes — a subsequent read returns 0 bytes (EOF).
    let mut buf = [0u8; 32];
    let n = c.read(&mut buf).unwrap_or(0);
    assert_eq!(n, 0, "expected EOF after queued QUIT inside EXEC");
}

#[test]
fn pipelined_command_after_exec_is_unaffected() {
    // After an EXEC (clean or dirty) the conn must accept the next
    // pipelined command at the correct seq. Stress: an aborted EXEC
    // followed immediately by a GET.
    let srv = Server::start(4);
    let mut c = srv.connect();
    let mut other = srv.connect();

    c.write_all(&req(&[b"SET", b"px", b"orig"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");

    c.write_all(&req(&[b"WATCH", b"px"])).unwrap();
    read_reply(&mut c, b"+OK\r\n");
    other.write_all(&req(&[b"SET", b"px", b"stomp"])).unwrap();
    read_reply(&mut other, b"+OK\r\n");

    // Pipeline MULTI/SET/EXEC + GET in one write.
    let mut batch = Vec::new();
    batch.extend_from_slice(&req(&[b"MULTI"]));
    batch.extend_from_slice(&req(&[b"SET", b"px", b"v"]));
    batch.extend_from_slice(&req(&[b"EXEC"]));
    batch.extend_from_slice(&req(&[b"GET", b"px"]));
    c.write_all(&batch).unwrap();

    read_reply(&mut c, b"+OK\r\n"); // MULTI
    read_reply(&mut c, b"+QUEUED\r\n"); // SET queued
    read_reply(&mut c, b"*-1\r\n"); // EXEC aborted
    read_reply(&mut c, b"$5\r\nstomp\r\n"); // pipelined GET still works
}
