//! A pipeline — commands sent in one write — on an eight-shard server keeps
//! its order across shards. A write sent before a multi-key or multi-step
//! command is seen by it; a command sent after a multi-step one (RENAME,
//! COPY, LMOVE, SMOVE, the copy-and-place stores) sees what it did; and the
//! connection never stops answering. Each case runs on many key pairs, so
//! the keys land on different shards in most of them.

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
        let dir = kevy_tmpdir::unique_dir("pipeorder");
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

/// `cmds` sent in one write: the last reply.
fn pipelined(c: &mut std::net::TcpStream, cmds: &[Vec<&[u8]>]) -> Vec<u8> {
    let bytes: Vec<u8> = cmds.iter().flat_map(|parts| req(parts)).collect();
    c.write_all(&bytes).unwrap();
    let mut last = Vec::new();
    for _ in cmds {
        last = read_reply(c);
    }
    last
}

fn run(name: &str, build: impl Fn(&str, &str) -> (Vec<Vec<Vec<u8>>>, Vec<u8>)) {
    let srv = Server::start();
    let mut c = srv.connect();
    let mut wrong = Vec::new();
    for i in 0..64 {
        let (a, b_) = (format!("{name}{i}a"), format!("{name}{i}b"));
        if shard(&a) == shard(&b_) {
            continue;
        }
        let (cmds, want) = build(&a, &b_);
        let parts: Vec<Vec<&[u8]>> =
            cmds.iter().map(|c| c.iter().map(Vec::as_slice).collect()).collect();
        let got = pipelined(&mut c, &parts);
        if got != want {
            wrong.push(String::from_utf8_lossy(&got).into_owned());
        }
    }
    assert!(
        wrong.is_empty(),
        "{name}: {} pipelines answered out of order: {:?}",
        wrong.len(),
        &wrong[..wrong.len().min(3)]
    );
}

fn cmd(parts: &[&str]) -> Vec<Vec<u8>> {
    parts.iter().map(|p| p.as_bytes().to_vec()).collect()
}

#[test]
fn a_write_is_seen_by_the_multi_key_command_after_it() {
    run("w-rename", |a, b| {
        (vec![cmd(&["SET", a, "v"]), cmd(&["RENAME", a, b])], b"+OK\r\n".to_vec())
    });
    run("w-copy", |a, b| (vec![cmd(&["SET", a, "v"]), cmd(&["COPY", a, b])], b":1\r\n".to_vec()));
    run("w-smove", |a, b| {
        (vec![cmd(&["SADD", a, "x"]), cmd(&["SMOVE", a, b, "x"])], b":1\r\n".to_vec())
    });
    run("w-msetnx", |a, b| {
        (vec![cmd(&["SET", a, "v"]), cmd(&["MSETNX", a, "1", b, "2"])], b":0\r\n".to_vec())
    });
    run("w-lcs", |a, b| {
        (
            vec![cmd(&["SET", a, "hello"]), cmd(&["SET", b, "yellow"]), cmd(&["LCS", a, b])],
            b"$4\r\nello\r\n".to_vec(),
        )
    });
    run("w-zmpop", |a, b| {
        (
            vec![cmd(&["ZADD", b, "1", "v"]), cmd(&["ZMPOP", "2", a, b, "MIN"])],
            format!("*2\r\n${}\r\n{b}\r\n*1\r\n*2\r\n$1\r\nv\r\n$1\r\n1\r\n", b.len()).into_bytes(),
        )
    });
}

#[test]
fn a_command_after_a_multi_step_one_sees_what_it_did() {
    run("r-rename", |a, b| {
        (
            vec![cmd(&["SET", a, "v"]), cmd(&["RENAME", a, b]), cmd(&["GET", b])],
            b"$1\r\nv\r\n".to_vec(),
        )
    });
    run("r-rename-set", |a, b| {
        (
            vec![
                cmd(&["SET", a, "old"]),
                cmd(&["RENAME", a, b]),
                cmd(&["SET", b, "new"]),
                cmd(&["GET", b]),
            ],
            b"$3\r\nnew\r\n".to_vec(),
        )
    });
    run("r-copy", |a, b| {
        (
            vec![cmd(&["SET", a, "v"]), cmd(&["COPY", a, b]), cmd(&["GET", b])],
            b"$1\r\nv\r\n".to_vec(),
        )
    });
    run("r-smove", |a, b| {
        (
            vec![cmd(&["SADD", a, "x"]), cmd(&["SMOVE", a, b, "x"]), cmd(&["SCARD", b])],
            b":1\r\n".to_vec(),
        )
    });
    run("r-lmove", |a, b| {
        (
            vec![cmd(&["RPUSH", a, "v"]), cmd(&["LMOVE", a, b, "LEFT", "LEFT"]), cmd(&["LLEN", b])],
            b":1\r\n".to_vec(),
        )
    });
    run("r-zrangestore", |a, b| {
        (
            vec![
                cmd(&["ZADD", a, "1", "x"]),
                cmd(&["ZRANGESTORE", b, a, "0", "-1"]),
                cmd(&["ZCARD", b]),
            ],
            b":1\r\n".to_vec(),
        )
    });
    // EXEC checks the versions WATCH collected, and what EXEC ran is seen
    run("r-watch-exec", |a, _| {
        let cmds = vec![
            cmd(&["SET", a, "1"]),
            cmd(&["WATCH", a]),
            cmd(&["MULTI"]),
            cmd(&["INCR", a]),
            cmd(&["EXEC"]),
            cmd(&["GET", a]),
        ];
        (cmds, b"$1\r\n2\r\n".to_vec())
    });
    run("r-zunionstore", |a, b| {
        (
            vec![cmd(&["ZADD", a, "1", "x"]), cmd(&["ZUNIONSTORE", b, "1", a]), cmd(&["ZCARD", b])],
            b":1\r\n".to_vec(),
        )
    });
}

/// A connection that goes away while its commands wait behind a multi-step
/// one leaves nothing behind: the server answers the next connection.
#[test]
fn a_held_connection_may_close() {
    let srv = Server::start();
    for i in 0..32 {
        let mut c = srv.connect();
        let (a, b) = (format!("gone{i}a"), format!("gone{i}b"));
        let cmds = [
            req(&[b"SET", a.as_bytes(), b"v"]),
            req(&[b"RENAME", a.as_bytes(), b.as_bytes()]),
            req(&[b"GET", b.as_bytes()]),
        ]
        .concat();
        c.write_all(&cmds).unwrap();
    }
    let mut c = srv.connect();
    c.write_all(&req(&[b"PING"])).unwrap();
    assert_eq!(read_reply(&mut c), b"+PONG\r\n");
}

/// A cross-shard LMOVE answers into its own reply slot, not into the slot
/// of a command before it that is still waiting on another shard.
#[test]
fn a_move_answers_in_its_own_place() {
    let srv = Server::start();
    let mut c = srv.connect();
    for i in 0..64 {
        let (k, a, b) = (format!("place{i}k"), format!("place{i}a"), format!("place{i}b"));
        c.write_all(&req(&[b"SET", k.as_bytes(), b"x"])).unwrap();
        read_reply(&mut c);
        let get = req(&[b"GET", k.as_bytes()]);
        let lmove = req(&[b"LMOVE", a.as_bytes(), b.as_bytes(), b"LEFT", b"LEFT"]);
        c.write_all(&[get.clone(), get.clone(), get, lmove].concat()).unwrap();
        let replies: Vec<Vec<u8>> = (0..4).map(|_| read_reply(&mut c)).collect();
        assert_eq!(
            replies,
            [&b"$1\r\nx\r\n"[..], b"$1\r\nx\r\n", b"$1\r\nx\r\n", b"$-1\r\n"],
            "{i}"
        );
    }
}

/// A blocked command holds the commands after it, as Redis holds a blocked
/// client: they run, and answer, once it has.
#[test]
fn commands_after_a_blocked_one_wait_for_it() {
    let srv = Server::start();
    let mut c = srv.connect();
    for i in 0..8 {
        let (q, k) = (format!("blk{i}q"), format!("blk{i}k"));
        let cmds = [
            req(&[b"BLPOP", q.as_bytes(), b"0.05"]),
            req(&[b"RPUSH", q.as_bytes(), b"x"]),
            req(&[b"SET", k.as_bytes(), b"1"]),
            req(&[b"GET", k.as_bytes()]),
        ];
        c.write_all(&cmds.concat()).unwrap();
        let replies: Vec<Vec<u8>> = (0..4).map(|_| read_reply(&mut c)).collect();
        // the push comes after the pop gave up: it does not serve it
        assert_eq!(replies, [&b"*-1\r\n"[..], b":1\r\n", b"+OK\r\n", b"$1\r\n1\r\n"], "{i}");
    }
}
