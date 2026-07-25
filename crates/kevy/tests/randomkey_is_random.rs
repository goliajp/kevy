//! RANDOMKEY returns an arbitrary key. It returned two fixed bugs stacked.
//!
//! Shard-side, the old path was `collect_keys(None, Some(1))` — the first key
//! in hash-bucket order, the same one every call. And the reducer then took
//! `acc.first()`: the FIRST SHARD's candidate, always. Net effect on a
//! multi-shard server: RANDOMKEY could only ever return one specific key per
//! shard-0 keyspace, and a key living on any other shard could not be returned
//! at all. "Do not use it for sampling" was written in the compatibility notes
//! as though documenting that made it acceptable.
//!
//! Now each shard draws from a random slot, and the origin folds the candidates
//! through a weighted reservoir (weight = the shard's key count), which makes
//! every key in the whole keyspace exactly equally likely.
//!
//! The statistical assertions here are loose on purpose — they must never flake
//! — but they still fail the old implementation by an order of magnitude.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

static START_GATE: Mutex<()> = Mutex::new(());

/// Eight shards, so "only shard 0's keys can win" is visibly different from
/// "every key can win".
const SHARDS: usize = 8;

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

fn read_reply(c: &mut std::net::TcpStream) -> Vec<u8> {
    let mut out = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        c.read_exact(&mut byte).unwrap();
        out.push(byte[0]);
        if out.len() >= 2 && out[out.len() - 2] == b'\r' && out[out.len() - 1] == b'\n' {
            break;
        }
    }
    match out[0] {
        b'+' | b'-' | b':' => out,
        b'$' => {
            let n: i64 = std::str::from_utf8(&out[1..out.len() - 2]).unwrap().parse().unwrap();
            if n < 0 {
                return out;
            }
            let mut body = vec![0u8; n as usize + 2];
            c.read_exact(&mut body).unwrap();
            out.extend_from_slice(&body);
            out
        }
        b'*' => {
            let n: i64 = std::str::from_utf8(&out[1..out.len() - 2]).unwrap().parse().unwrap();
            for _ in 0..n.max(0) {
                out.extend_from_slice(&read_reply(c));
            }
            out
        }
        _ => out,
    }
}

fn call(c: &mut std::net::TcpStream, parts: &[&[u8]]) -> Vec<u8> {
    c.write_all(&req(parts)).unwrap();
    read_reply(c)
}

/// The bulk payload of a `$`-reply, or None for nil.
fn bulk(reply: &[u8]) -> Option<Vec<u8>> {
    if reply.starts_with(b"$-1") {
        return None;
    }
    let head_end = reply.iter().position(|&b| b == b'\n').unwrap() + 1;
    Some(reply[head_end..reply.len() - 2].to_vec())
}

struct Server {
    port: u16,
    dir: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start(nshards: usize) -> Self {
        let _gate = START_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let port = free_port();
        let dir = std::env::temp_dir().join(format!(
            "kevy-randomkey-{}-{}",
            std::process::id(),
            port
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let dir_thread = dir.clone();
        let handle = std::thread::spawn(move || {
            let rt = kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(nshards))
                .bind([127, 0, 0, 1], port)
                .shards(nshards)
                .with_data_dir(dir_thread);
            rt.run(stop_thread).unwrap();
        });
        for _ in 0..200 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        Self { port, dir, stop, handle: Some(handle) }
    }

    fn connect(&self) -> std::net::TcpStream {
        let s = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
        s
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = std::net::TcpStream::connect(("127.0.0.1", self.port));
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The headline: draws must range over the keyspace, not orbit one key.
///
/// 160 keys spread over 8 shards put ~20 on shard 0. The old reducer could only
/// ever answer with shard-0's first-in-bucket-order key — ONE distinct value.
/// Even the old code plus a fixed reducer could reach at most ~20. Requiring 45
/// distinct keys from 400 draws is far above both and far below what a real
/// uniform draw produces (~149 expected).
#[test]
fn randomkey_ranges_over_the_whole_keyspace() {
    let srv = Server::start(SHARDS);
    let mut c = srv.connect();
    for i in 0..160 {
        let k = format!("k:{i:03}");
        assert!(call(&mut c, &[b"SET", k.as_bytes(), b"v"]).starts_with(b"+OK"));
    }

    let mut seen = std::collections::HashSet::new();
    for _ in 0..400 {
        let key = bulk(&call(&mut c, &[b"RANDOMKEY"])).expect("db is not empty");
        assert!(key.starts_with(b"k:"), "returned a key we never wrote");
        seen.insert(key);
    }
    assert!(
        seen.len() > 45,
        "400 draws over 160 keys produced only {} distinct keys — RANDOMKEY is \
         stuck on one shard or one bucket again",
        seen.len()
    );
}

/// Two runs of the same server over the same data must not draw the same
/// sequence. (The store RNG is clock-seeded; two servers ARE two seeds.)
#[test]
fn two_servers_disagree_about_arbitrary() {
    let take = |srv: &Server| {
        let mut c = srv.connect();
        for i in 0..64 {
            let k = format!("k:{i:02}");
            call(&mut c, &[b"SET", k.as_bytes(), b"v"]);
        }
        (0..24)
            .map(|_| bulk(&call(&mut c, &[b"RANDOMKEY"])).expect("non-empty"))
            .collect::<Vec<_>>()
    };
    let a = {
        let srv = Server::start(SHARDS);
        take(&srv)
    };
    let b = {
        let srv = Server::start(SHARDS);
        take(&srv)
    };
    assert_ne!(a, b, "two servers drew the identical 24-key sequence");
}

#[test]
fn randomkey_on_an_empty_db_is_nil() {
    let srv = Server::start(SHARDS);
    let mut c = srv.connect();
    assert!(
        call(&mut c, &[b"RANDOMKEY"]).starts_with(b"$-1"),
        "empty keyspace must answer nil"
    );
}

/// One key total: every draw must find it, whichever shard it landed on. This
/// pins the reservoir's bookkeeping — a fold that mis-weights an empty shard
/// would sometimes answer nil here.
#[test]
fn a_single_key_is_always_found() {
    let srv = Server::start(SHARDS);
    let mut c = srv.connect();
    call(&mut c, &[b"SET", b"only", b"v"]);
    for _ in 0..50 {
        let key = bulk(&call(&mut c, &[b"RANDOMKEY"])).expect("the key exists");
        assert_eq!(key, b"only");
    }
}
