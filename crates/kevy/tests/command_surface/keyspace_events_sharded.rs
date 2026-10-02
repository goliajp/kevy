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

/// The string and key commands, the same way: `set` and `expire` for a
/// write with a deadline, `del` for one whose deadline had passed, `incrby`
/// for every counter step, `rename_*` and `copy_to` across shards too.
const SCRIPT_STRINGS: &[(&str, &[&str])] = &[
    ("SETEX k 100 v", &["set(k)", "expire(k)"]),
    ("PSETEX k 100000 v", &["set(k)", "expire(k)"]),
    ("SETNX k v", &[]),
    ("SETNX k2 v", &["set(k2)"]),
    ("GETSET k w", &["set(k)"]),
    ("GETDEL k", &["del(k)"]),
    ("GETDEL k", &[]),
    ("APPEND k x", &["append(k)"]),
    ("INCR n", &["incrby(n)"]),
    ("INCRBY n 2", &["incrby(n)"]),
    ("DECR n", &["incrby(n)"]),
    ("DECRBY n 2", &["incrby(n)"]),
    ("INCRBYFLOAT n 1.5", &["incrbyfloat(n)"]),
    ("SETRANGE k 3 yy", &["setrange(k)"]),
    ("SETBIT b 3 1", &["setbit(b)"]),
    ("EXPIRE k 100", &["expire(k)"]),
    ("PEXPIRE k 100000", &["expire(k)"]),
    ("EXPIREAT k 4102444800", &["expire(k)"]),
    ("PEXPIREAT k 4102444800000", &["expire(k)"]),
    ("PERSIST k", &["persist(k)"]),
    ("PERSIST k", &[]),
    ("EXPIRE nokey 100", &[]),
    ("EXPIRE k 0", &["del(k)"]),
    ("SET k v", &["set(k)"]),
    ("PEXPIRE k -5", &["del(k)"]),
    ("SET k v", &["set(k)"]),
    ("EXPIREAT k 1", &["del(k)"]),
    ("MSET a 1 b 2", &["set(a)", "set(b)"]),
    ("DEL a", &["del(a)"]),
    ("UNLINK b", &["del(b)"]),
    ("RENAME k2 k3", &["rename_from(k2)", "rename_to(k3)"]),
    ("RENAMENX k3 k4", &["rename_from(k3)", "rename_to(k4)"]),
    ("COPY k4 k5", &["copy_to(k5)"]),
    ("SET s v", &["set(s)"]),
    ("LPUSH l a", &["lpush(l)"]),
    ("SET l v", &["set(l)"]),
    ("SET l v GET", &["set(l)"]),
    ("SET k v", &["set(k)"]),
    ("SET k v EX 100", &["set(k)", "expire(k)"]),
    ("SET k v KEEPTTL", &["set(k)"]),
    ("SET k v PXAT 4102444800123", &["set(k)", "expire(k)"]),
    ("SET k v EXAT 1", &["del(k)"]),
    ("SET k v EXAT 1", &[]),
    ("SET k v NX", &["set(k)"]),
    ("SET k w XX GET", &["set(k)"]),
    ("SET k w IFEQ w", &["set(k)"]),
    ("SET k x IFEQ nope", &[]),
    ("SET k v EX 100", &["set(k)", "expire(k)"]),
    ("GETEX k EX 50", &["expire(k)"]),
    ("GETEX k PERSIST", &["persist(k)"]),
    ("GETEX k PERSIST", &[]),
    ("GETEX k", &[]),
    ("GETEX k EXAT 1", &["del(k)"]),
    ("SET k v", &["set(k)"]),
    ("EXPIRE k 100 NX", &["expire(k)"]),
    ("EXPIRE k 100 NX", &[]),
    ("EXPIRE k 50 LT", &["expire(k)"]),
    ("EXPIRE k -1 LT", &["del(k)"]),
    ("SET k v", &["set(k)"]),
    ("PEXPIREAT k 1", &["del(k)"]),
    ("RPUSH l a b", &[]),
    ("LPOP l 0", &[]),
    ("LPOP l 5", &[]),
    ("SET s0 v", &["set(s0)"]),
    ("RENAME s0 r0", &["rename_from(s0)", "rename_to(r0)"]),
    ("RENAMENX r0 n0", &["rename_from(r0)", "rename_to(n0)"]),
    ("COPY n0 c0", &["copy_to(c0)"]),
    ("COPY n0 c0", &[]),
    ("COPY n0 c0 REPLACE", &["copy_to(c0)"]),
    ("SET x0 v", &["set(x0)"]),
    ("RENAMENX n0 x0", &[]),
    ("SET s1 v", &["set(s1)"]),
    ("RENAME s1 r1", &["rename_from(s1)", "rename_to(r1)"]),
    ("RENAMENX r1 n1", &["rename_from(r1)", "rename_to(n1)"]),
    ("COPY n1 c1", &["copy_to(c1)"]),
    ("COPY n1 c1", &[]),
    ("COPY n1 c1 REPLACE", &["copy_to(c1)"]),
    ("SET x1 v", &["set(x1)"]),
    ("RENAMENX n1 x1", &[]),
    ("SET s2 v", &["set(s2)"]),
    ("RENAME s2 r2", &["rename_from(s2)", "rename_to(r2)"]),
    ("RENAMENX r2 n2", &["rename_from(r2)", "rename_to(n2)"]),
    ("COPY n2 c2", &["copy_to(c2)"]),
    ("COPY n2 c2", &[]),
    ("COPY n2 c2 REPLACE", &["copy_to(c2)"]),
    ("SET x2 v", &["set(x2)"]),
    ("RENAMENX n2 x2", &[]),
    ("SET s3 v", &["set(s3)"]),
    ("RENAME s3 r3", &["rename_from(s3)", "rename_to(r3)"]),
    ("RENAMENX r3 n3", &["rename_from(r3)", "rename_to(n3)"]),
    ("COPY n3 c3", &["copy_to(c3)"]),
    ("COPY n3 c3", &[]),
    ("COPY n3 c3 REPLACE", &["copy_to(c3)"]),
    ("SET x3 v", &["set(x3)"]),
    ("RENAMENX n3 x3", &[]),
    ("SET s4 v", &["set(s4)"]),
    ("RENAME s4 r4", &["rename_from(s4)", "rename_to(r4)"]),
    ("RENAMENX r4 n4", &["rename_from(r4)", "rename_to(n4)"]),
    ("COPY n4 c4", &["copy_to(c4)"]),
    ("COPY n4 c4", &[]),
    ("COPY n4 c4 REPLACE", &["copy_to(c4)"]),
    ("SET x4 v", &["set(x4)"]),
    ("RENAMENX n4 x4", &[]),
    ("SET s5 v", &["set(s5)"]),
    ("RENAME s5 r5", &["rename_from(s5)", "rename_to(r5)"]),
    ("RENAMENX r5 n5", &["rename_from(r5)", "rename_to(n5)"]),
    ("COPY n5 c5", &["copy_to(c5)"]),
    ("COPY n5 c5", &[]),
    ("COPY n5 c5 REPLACE", &["copy_to(c5)"]),
    ("SET x5 v", &["set(x5)"]),
    ("RENAMENX n5 x5", &[]),
    ("SET s6 v", &["set(s6)"]),
    ("RENAME s6 r6", &["rename_from(s6)", "rename_to(r6)"]),
    ("RENAMENX r6 n6", &["rename_from(r6)", "rename_to(n6)"]),
    ("COPY n6 c6", &["copy_to(c6)"]),
    ("COPY n6 c6", &[]),
    ("COPY n6 c6 REPLACE", &["copy_to(c6)"]),
    ("SET x6 v", &["set(x6)"]),
    ("RENAMENX n6 x6", &[]),
    ("SET s7 v", &["set(s7)"]),
    ("RENAME s7 r7", &["rename_from(s7)", "rename_to(r7)"]),
    ("RENAMENX r7 n7", &["rename_from(r7)", "rename_to(n7)"]),
    ("COPY n7 c7", &["copy_to(c7)"]),
    ("COPY n7 c7", &[]),
    ("COPY n7 c7 REPLACE", &["copy_to(c7)"]),
    ("SET x7 v", &["set(x7)"]),
    ("RENAMENX n7 x7", &[]),
];

fn run_script(nshards: usize, script: &[(&str, &[&str])]) {
    let srv = Server::start(nshards);
    let mut sub = srv.connect();
    sub.write_all(&req(&[b"PSUBSCRIBE", b"__keyevent@0__:*"])).unwrap();
    drain(&mut sub, Duration::from_millis(200));
    let mut c = srv.connect();
    for (cmd, want) in script {
        let parts: Vec<&[u8]> = cmd.split(' ').map(str::as_bytes).collect();
        c.write_all(&req(&parts)).unwrap();
        drain(&mut c, Duration::from_millis(100));
        let got = events(&drain(&mut sub, Duration::from_millis(150)));
        assert_eq!(got, *want, "{nshards} shards: {cmd}");
    }
}

#[test]
fn events_match_redis_on_one_shard() {
    run_script(1, SCRIPT);
    run_script(1, SCRIPT_STRINGS);
}

#[test]
fn events_match_redis_across_shards() {
    run_script(4, SCRIPT);
    run_script(4, SCRIPT_STRINGS);
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
