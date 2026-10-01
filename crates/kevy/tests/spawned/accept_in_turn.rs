//! Connections to the shared port go to the shard holding the fewest, not
//! where the kernel's hash of each connection's ports sends them: 48
//! connections on four shards are 12 on each, give or take the readiness
//! probe's own connection while it is still being closed.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::time::Duration;

fn call(s: &mut TcpStream, cmd: &[&str]) -> String {
    let mut req = format!("*{}\r\n", cmd.len());
    for a in cmd {
        req.push_str(&format!("${}\r\n{a}\r\n", a.len()));
    }
    s.write_all(req.as_bytes()).unwrap();
    let mut r = BufReader::new(s.try_clone().unwrap());
    let mut head = String::new();
    r.read_line(&mut head).unwrap();
    match head.strip_prefix('$') {
        Some(len) => {
            let mut body = vec![0u8; len.trim().parse::<usize>().unwrap() + 2];
            r.read_exact(&mut body).unwrap();
            String::from_utf8(body).unwrap()
        }
        None => head,
    }
}

#[test]
fn shared_port_connections_are_spread_evenly_over_the_shards() {
    const SHARDS: usize = 4;
    let port = kevy_testnet::free_port();
    let dir = kevy_tmpdir::TmpDir::new("accept-in-turn");
    let mut server = Command::new(env!("CARGO_BIN_EXE_kevy"))
        .args(["--port", &port.to_string(), "--threads", &SHARDS.to_string(), "--no-aof", "--dir"])
        .arg(dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn kevy");
    kevy_testnet::assert_listening(port, "kevy");
    let mut conns: Vec<TcpStream> = (0..48)
        .map(|_| {
            let s = TcpStream::connect(("127.0.0.1", port)).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            s
        })
        .collect();
    // every connection serves, wherever it was passed to
    for (i, s) in conns.iter_mut().enumerate() {
        assert_eq!(call(s, &["SET", &format!("k{i}"), "v"]), "+OK\r\n");
    }
    let list = call(&mut conns[0], &["CLIENT", "LIST"]);
    let mut per_shard = [0usize; SHARDS];
    for id in list.split_whitespace().filter_map(|f| f.strip_prefix("id=")) {
        let id: usize = id.parse().unwrap();
        per_shard[(id - 1) % SHARDS] += 1;
    }
    let _ = server.kill();
    let _ = server.wait();
    let (lo, hi) = (per_shard.iter().min().unwrap(), per_shard.iter().max().unwrap());
    assert!(hi - lo <= 1 && per_shard.iter().sum::<usize>() == 48, "{per_shard:?} {list}");
}
