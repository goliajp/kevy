//! The multi-key pops — `ZMPOP` / `LMPOP` and the blocking `BZMPOP` /
//! `BLMPOP` / `BZPOPMAX` / `BLMOVE` — on an eight-shard server, where the
//! keys a call names routinely live on different cores. Each test picks its
//! keys so that they do, and asserts what is left in the keyspace as well
//! as the reply.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

static START_GATE: Mutex<()> = Mutex::new(());
const SHARDS: usize = 8;

fn req(parts: &[&[u8]]) -> Vec<u8> {
    let mut v = format!("*{}\r\n", parts.len()).into_bytes();
    for p in parts {
        v.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
        v.extend_from_slice(p);
        v.extend_from_slice(b"\r\n");
    }
    v
}

fn read_reply(c: &mut std::net::TcpStream) -> Vec<u8> {
    let mut out = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        c.read_exact(&mut byte).unwrap();
        out.push(byte[0]);
        if out.ends_with(b"\r\n") {
            break;
        }
    }
    let n = || std::str::from_utf8(&out[1..out.len() - 2]).unwrap().parse::<i64>().unwrap();
    match out[0] {
        b'$' if n() >= 0 => {
            let mut body = vec![0u8; n() as usize + 2];
            c.read_exact(&mut body).unwrap();
            out.extend_from_slice(&body);
        }
        b'*' => {
            for _ in 0..n().max(0) {
                let inner = read_reply(c);
                out.extend_from_slice(&inner);
            }
        }
        _ => {}
    }
    out
}

fn call(c: &mut std::net::TcpStream, parts: &[&[u8]]) -> Vec<u8> {
    c.write_all(&req(parts)).unwrap();
    read_reply(c)
}

struct Server {
    port: u16,
    dir: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start() -> Self {
        let _gate = START_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let port = kevy_testnet::free_port();
        let dir = kevy_tmpdir::unique_dir("mpop");
        let stop = Arc::new(AtomicBool::new(false));
        let (stop_thread, dir_thread) = (stop.clone(), dir.clone());
        let handle = std::thread::spawn(move || {
            kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(SHARDS))
                .bind([127, 0, 0, 1], port)
                .shards(SHARDS)
                .with_data_dir(dir_thread)
                .run(stop_thread)
                .unwrap();
        });
        kevy_testnet::assert_listening(port, "the server under test");
        Self { port, dir, stop, handle: Some(handle) }
    }

    fn connect(&self) -> std::net::TcpStream {
        let s = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
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

fn shard(key: &str) -> usize {
    kevy_rt::shard_of_key(key.as_bytes(), SHARDS, kevy_persist::Routing::KevyHash)
}

/// `n` keys named `{prefix}N`, each on a different shard.
fn apart(prefix: &str, n: usize) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for i in 0.. {
        let k = format!("{prefix}{i}");
        if keys.iter().all(|o| shard(o) != shard(&k)) {
            keys.push(k);
            if keys.len() == n {
                return keys;
            }
        }
    }
    unreachable!()
}

fn b(s: &str) -> &[u8] {
    s.as_bytes()
}

/// The first key holding something answers, in the order the call names
/// them — never a later key that happens to live on the conn's own shard.
#[test]
fn zmpop_takes_from_the_first_key_that_holds_something() {
    let srv = Server::start();
    let mut c = srv.connect();
    let k = apart("zm", 3);
    call(&mut c, &[b"ZADD", b(&k[1]), b"1", b"a", b"2", b"b"]);
    call(&mut c, &[b"ZADD", b(&k[2]), b"9", b"z"]);
    let r = call(&mut c, &[b"ZMPOP", b"3", b(&k[0]), b(&k[1]), b(&k[2]), b"MAX", b"COUNT", b"5"]);
    let want = format!(
        "*2\r\n${}\r\n{}\r\n*2\r\n*2\r\n$1\r\nb\r\n$1\r\n2\r\n*2\r\n$1\r\na\r\n$1\r\n1\r\n",
        k[1].len(),
        k[1]
    );
    assert_eq!(String::from_utf8_lossy(&r), want);
    assert_eq!(call(&mut c, &[b"EXISTS", b(&k[1])]), b":0\r\n", "the popped key is gone");
    assert_eq!(call(&mut c, &[b"ZCARD", b(&k[2])]), b":1\r\n", "a later key is untouched");
    let none = call(&mut c, &[b"ZMPOP", b"2", b(&k[0]), b(&k[1]), b"MIN"]);
    assert_eq!(none, b"*-1\r\n");
}

#[test]
fn lmpop_takes_from_the_first_key_and_reports_a_wrong_type_in_order() {
    let srv = Server::start();
    let mut c = srv.connect();
    let k = apart("lm", 3);
    call(&mut c, &[b"RPUSH", b(&k[1]), b"x", b"y", b"z"]);
    call(&mut c, &[b"RPUSH", b(&k[2]), b"q"]);
    let r = call(&mut c, &[b"LMPOP", b"3", b(&k[0]), b(&k[1]), b(&k[2]), b"RIGHT", b"COUNT", b"2"]);
    let want = format!("*2\r\n${}\r\n{}\r\n*2\r\n$1\r\nz\r\n$1\r\ny\r\n", k[1].len(), k[1]);
    assert_eq!(String::from_utf8_lossy(&r), want);
    assert_eq!(call(&mut c, &[b"LRANGE", b(&k[1]), b"0", b"-1"]), b"*1\r\n$1\r\nx\r\n");
    call(&mut c, &[b"SET", b(&k[0]), b"str"]);
    let r = call(&mut c, &[b"LMPOP", b"2", b(&k[0]), b(&k[2]), b"LEFT"]);
    assert!(r.starts_with(b"-WRONGTYPE"), "the first key is checked first: {r:?}");
    assert_eq!(call(&mut c, &[b"LLEN", b(&k[2])]), b":1\r\n", "nothing taken past the error");
}

#[test]
fn co_located_keys_pop_on_their_shard() {
    let srv = Server::start();
    let mut c = srv.connect();
    call(&mut c, &[b"ZADD", b"{t}b", b"1", b"m"]);
    let r = call(&mut c, &[b"ZMPOP", b"2", b"{t}a", b"{t}b", b"MIN"]);
    assert_eq!(r, b"*2\r\n$4\r\n{t}b\r\n*1\r\n*2\r\n$1\r\nm\r\n$1\r\n1\r\n");
}

/// A parked `BZMPOP` wakes for a write to any of its keys, on whichever
/// shard that key lives, and pops the count from the asked-for end.
#[test]
fn bzmpop_parks_and_wakes_for_a_key_on_another_shard() {
    let srv = Server::start();
    let (mut waiter, mut writer) = (srv.connect(), srv.connect());
    let k = apart("bz", 2);
    waiter
        .write_all(&req(&[b"BZMPOP", b"5", b"2", b(&k[0]), b(&k[1]), b"MAX", b"COUNT", b"2"]))
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    call(&mut writer, &[b"ZADD", b(&k[1]), b"1", b"a", b"2", b"b", b"3", b"c"]);
    let want = format!(
        "*2\r\n${}\r\n{}\r\n*2\r\n*2\r\n$1\r\nc\r\n$1\r\n3\r\n*2\r\n$1\r\nb\r\n$1\r\n2\r\n",
        k[1].len(),
        k[1]
    );
    assert_eq!(String::from_utf8_lossy(&read_reply(&mut waiter)), want);
    assert_eq!(call(&mut writer, &[b"ZRANGE", b(&k[1]), b"0", b"-1"]), b"*1\r\n$1\r\na\r\n");
}

#[test]
fn blmpop_serves_at_once_from_a_remote_key_with_data() {
    let srv = Server::start();
    let mut c = srv.connect();
    let k = apart("bl", 2);
    call(&mut c, &[b"RPUSH", b(&k[1]), b"1", b"2", b"3"]);
    let r = call(&mut c, &[b"BLMPOP", b"1", b"2", b(&k[0]), b(&k[1]), b"LEFT", b"COUNT", b"2"]);
    let want = format!("*2\r\n${}\r\n{}\r\n*2\r\n$1\r\n1\r\n$1\r\n2\r\n", k[1].len(), k[1]);
    assert_eq!(String::from_utf8_lossy(&r), want);
    assert_eq!(call(&mut c, &[b"LRANGE", b(&k[1]), b"0", b"-1"]), b"*1\r\n$1\r\n3\r\n");
}

#[test]
fn bzpopmax_takes_the_top_member_of_a_remote_key() {
    let srv = Server::start();
    let mut c = srv.connect();
    let k = apart("bx", 2);
    call(&mut c, &[b"ZADD", b(&k[1]), b"1", b"lo", b"5", b"hi"]);
    let r = call(&mut c, &[b"BZPOPMAX", b(&k[0]), b(&k[1]), b"1"]);
    let want = format!("*3\r\n${}\r\n{}\r\n$2\r\nhi\r\n$1\r\n5\r\n", k[1].len(), k[1]);
    assert_eq!(String::from_utf8_lossy(&r), want);
}

/// Both ends of a parked `BLMOVE` hold when source and destination live
/// on different shards.
#[test]
fn blmove_parks_then_moves_between_the_named_ends_across_shards() {
    let srv = Server::start();
    let (mut waiter, mut writer) = (srv.connect(), srv.connect());
    let k = apart("mv", 2);
    call(&mut writer, &[b"RPUSH", b(&k[1]), b"old"]);
    waiter.write_all(&req(&[b"BLMOVE", b(&k[0]), b(&k[1]), b"RIGHT", b"LEFT", b"5"])).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    call(&mut writer, &[b"RPUSH", b(&k[0]), b"a", b"b"]);
    assert_eq!(read_reply(&mut waiter), b"$1\r\nb\r\n");
    assert_eq!(
        call(&mut writer, &[b"LRANGE", b(&k[1]), b"0", b"-1"]),
        b"*2\r\n$1\r\nb\r\n$3\r\nold\r\n"
    );
    assert_eq!(call(&mut writer, &[b"LRANGE", b(&k[0]), b"0", b"-1"]), b"*1\r\n$1\r\na\r\n");
}

/// Every blocking timeout is the null array under RESP2, the moves'
/// included.
#[test]
fn blocking_timeouts_answer_the_null_array() {
    let srv = Server::start();
    let mut c = srv.connect();
    let k = apart("to", 2);
    for cmd in [
        vec![b"BZMPOP" as &[u8], b"0.05", b"2", b(&k[0]), b(&k[1]), b"MIN"],
        vec![b"BLMPOP", b"0.05", b"1", b(&k[0]), b"LEFT"],
        vec![b"BZPOPMAX", b(&k[0]), b"0.05"],
        vec![b"BLMOVE", b(&k[0]), b(&k[1]), b"LEFT", b"LEFT", b"0.05"],
        vec![b"BRPOPLPUSH", b(&k[0]), b(&k[1]), b"0.05"],
    ] {
        assert_eq!(call(&mut c, &cmd), b"*-1\r\n", "{:?}", String::from_utf8_lossy(cmd[0]));
    }
}

/// A watched key of the wrong type answers WRONGTYPE at once, whichever
/// shard it lives on, rather than blocking until the timeout.
#[test]
fn a_wrong_type_key_answers_at_once_instead_of_blocking() {
    let srv = Server::start();
    let mut c = srv.connect();
    let k = apart("wt", 2);
    call(&mut c, &[b"SET", b(&k[1]), b"str"]);
    for cmd in [
        vec![b"BLPOP" as &[u8], b(&k[0]), b(&k[1]), b"3"],
        vec![b"BZPOPMIN", b(&k[0]), b(&k[1]), b"3"],
        vec![b"BZMPOP", b"3", b"2", b(&k[0]), b(&k[1]), b"MIN"],
    ] {
        let t0 = std::time::Instant::now();
        let r = call(&mut c, &cmd);
        assert!(r.starts_with(b"-WRONGTYPE"), "{:?}: {r:?}", String::from_utf8_lossy(cmd[0]));
        assert!(t0.elapsed() < Duration::from_secs(2), "it waited for the timeout");
    }
}
