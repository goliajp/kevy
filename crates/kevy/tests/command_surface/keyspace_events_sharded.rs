//! Keyspace events on a sharded server, against the events Redis 8.10.2
//! publishes for the same commands (captured from the real binary): the
//! pops and moves announce what they did — the end, the key it came from,
//! the push before the pop of a move, a `del` once a collection is
//! emptied — and a write that changed nothing announces nothing. Every
//! test runs on several shards: a subscriber lives on one shard, and an
//! event raised on another must still reach it.

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

fn req(parts: &[&[u8]]) -> Vec<u8> {
    let mut v = format!("*{}\r\n", parts.len()).into_bytes();
    for p in parts {
        v.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
        v.extend_from_slice(p);
        v.extend_from_slice(b"\r\n");
    }
    v
}

struct Server {
    port: u16,
    dir: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start(nshards: usize) -> Self {
        let port = kevy_testnet::free_port();
        let dir = kevy_tmpdir::unique_dir("keyevents");
        let mut cfg = kevy_config::Config::default();
        cfg.notification.notify_keyspace_events = "EA".to_string();
        let state = Arc::new(
            kevy::RuntimeState::new(Arc::new(cfg), std::path::PathBuf::new(), nshards).unwrap(),
        );
        let stop = Arc::new(AtomicBool::new(false));
        let (stop_thread, dir_thread) = (stop.clone(), dir.clone());
        let handle = std::thread::spawn(move || {
            kevy_rt::Runtime::builder(kevy::KevyCommands::with_state(state))
                .bind([127, 0, 0, 1], port)
                .shards(nshards)
                .with_data_dir(dir_thread)
                .run(stop_thread)
                .unwrap();
        });
        kevy_testnet::assert_listening(port, "the server under test");
        // every shard latches the flags on its tick (100 ms by default)
        std::thread::sleep(Duration::from_millis(500));
        Self { port, dir, stop, handle: Some(handle) }
    }

    fn connect(&self) -> std::net::TcpStream {
        std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap()
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

/// Everything that arrives within `wait`.
fn drain(s: &mut std::net::TcpStream, wait: Duration) -> Vec<u8> {
    s.set_read_timeout(Some(wait)).unwrap();
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    while let Ok(n) = s.read(&mut buf) {
        if n == 0 {
            break;
        }
        got.extend_from_slice(&buf[..n]);
    }
    got
}

/// `event(key)` for each pmessage frame in `bytes`, in order.
fn events(bytes: &[u8]) -> Vec<String> {
    let parts: Vec<&[u8]> =
        bytes.split(|b| *b == b'\n').map(|l| l.strip_suffix(b"\r").unwrap_or(l)).collect();
    let mut out = Vec::new();
    for (i, p) in parts.iter().enumerate() {
        if *p == b"pmessage" {
            let chan = String::from_utf8_lossy(parts[i + 4]);
            let key = String::from_utf8_lossy(parts[i + 6]);
            out.push(format!("{}({key})", chan.trim_start_matches("__keyevent@0__:")));
        }
    }
    out
}

/// `(command, the events Redis 8.10.2 published for it)`, in order.
const SCRIPT: &[(&str, &[&str])] = &[
    ("ZADD z 1 a 2 b 3 c", &["zadd(z)"]),
    ("ZPOPMIN z", &["zpopmin(z)"]),
    ("ZPOPMIN nokey", &[]),
    ("ZPOPMAX z", &["zpopmax(z)"]),
    ("BZPOPMIN z 0", &["zpopmin(z)", "del(z)"]),
    ("ZADD z 1 a 2 b", &["zadd(z)"]),
    ("ZMPOP 2 nokey z MAX COUNT 5", &["zpopmax(z)", "del(z)"]),
    ("RPUSH l a b c d", &["rpush(l)"]),
    ("BLPOP l 0", &["lpop(l)"]),
    ("LPOP nokey", &[]),
    ("LMPOP 1 l RIGHT COUNT 2", &["rpop(l)"]),
    ("RPUSH s1 x", &["rpush(s1)"]),
    ("BLMOVE s1 d1 LEFT RIGHT 0", &["rpush(d1)", "lpop(s1)", "del(s1)"]),
    ("SREM nokey m", &[]),
    ("SADD st m", &["sadd(st)"]),
    ("SPOP st", &["spop(st)", "del(st)"]),
    ("RPUSH s a b", &["rpush(s)"]),
    ("LMOVE s d RIGHT LEFT", &["lpush(d)", "rpop(s)"]),
    ("RPOPLPUSH s d", &["lpush(d)", "rpop(s)", "del(s)"]),
    ("RPOPLPUSH s d", &[]),
    ("LMOVE nokey d LEFT LEFT", &[]),
    ("BRPOPLPUSH d e 0", &["lpush(e)", "rpop(d)"]),
    ("RPUSH w x", &["rpush(w)"]),
    ("BLMOVE w w LEFT RIGHT 0", &["rpush(w)", "lpop(w)"]),
    ("ZADD z 1 a", &["zadd(z)"]),
    ("BZMPOP 0 1 z MIN", &["zpopmin(z)", "del(z)"]),
    ("RPUSH q 1", &["rpush(q)"]),
    ("BLMPOP 0 1 q RIGHT", &["rpop(q)", "del(q)"]),
    ("RPUSH l a", &["rpush(l)"]),
    ("LPUSHX l x", &["lpush(l)"]),
    ("RPUSHX l y z", &["rpush(l)"]),
    ("LPUSHX nokey a", &[]),
    ("HSET h f 1 g 2", &["hset(h)"]),
    ("HEXPIRE h 100 FIELDS 1 f", &["hexpire(h)"]),
    ("HEXPIREAT h 4102444800 FIELDS 1 f", &["hexpire(h)"]),
    ("HPEXPIRE h 100000 FIELDS 1 g", &["hexpire(h)"]),
    ("HPEXPIREAT h 4102444800000 NX FIELDS 1 f", &[]),
    ("HPERSIST h FIELDS 1 f", &["hpersist(h)"]),
    ("HEXPIREAT h 1 FIELDS 1 f", &["hdel(h)"]),
    ("HEXPIRE h 0 FIELDS 1 g", &["hdel(h)", "del(h)"]),
    ("HEXPIRE nokey 10 FIELDS 1 f", &[]),
    ("RPUSH src 3 1 2", &["rpush(src)"]),
    ("SORT src STORE sd", &["sortstore(sd)"]),
    ("SORT src STORE sd2 LIMIT 5 1", &[]),
    ("SET sd3 x", &["set(sd3)"]),
    ("SORT src STORE sd3 LIMIT 5 1", &["del(sd3)"]),
    ("SORT src STORE sd DESC GET # GET #", &["sortstore(sd)"]),
    ("SORT_RO src", &[]),
    ("SADD ss b a c", &["sadd(ss)"]),
    ("SORT ss STORE sd5 BY nosort", &["sortstore(sd5)"]),
];

fn run_script(nshards: usize) {
    let srv = Server::start(nshards);
    let mut sub = srv.connect();
    sub.write_all(&req(&[b"PSUBSCRIBE", b"__keyevent@0__:*"])).unwrap();
    drain(&mut sub, Duration::from_millis(200));
    let mut c = srv.connect();
    for (cmd, want) in SCRIPT {
        let parts: Vec<&[u8]> = cmd.split(' ').map(str::as_bytes).collect();
        c.write_all(&req(&parts)).unwrap();
        drain(&mut c, Duration::from_millis(100));
        let got = events(&drain(&mut sub, Duration::from_millis(150)));
        assert_eq!(got, *want, "{nshards} shards: {cmd}");
    }
}

#[test]
fn events_match_redis_on_one_shard() {
    run_script(1);
}

#[test]
fn events_match_redis_across_shards() {
    run_script(4);
}

/// An event raised on any shard reaches a subscriber on another. The
/// flush of the cross-shard batch skipped the events for years, because
/// the mask it reads was set only by PUBLISH.
#[test]
fn every_shards_events_reach_the_subscriber() {
    let srv = Server::start(8);
    let mut sub = srv.connect();
    sub.write_all(&req(&[b"PSUBSCRIBE", b"__keyevent@0__:*"])).unwrap();
    drain(&mut sub, Duration::from_millis(200));
    let mut c = srv.connect();
    for i in 0..32 {
        let key = format!("k{i}");
        c.write_all(&req(&[b"SET", key.as_bytes(), b"v"])).unwrap();
        drain(&mut c, Duration::from_millis(30));
        let got = events(&drain(&mut sub, Duration::from_millis(100)));
        assert_eq!(got, [format!("set({key})")], "shard of {key}");
    }
}
