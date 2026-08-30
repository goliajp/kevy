//! SPOP propagates its EFFECT, not its verb.
//!
//! v4 made SPOP genuinely random — which turned a verb-logged `SPOP`
//! frame into a divergence bomb: an AOF restart (or a replica) replaying
//! `SPOP key n` draws its own random members and ends up with a
//! different remaining set than the process that answered the client.
//! The fix records `SREM key <popped…>` (the members actually removed)
//! instead, and records nothing for a pop that removed nothing —
//! exactly Redis's propagation rule, and what `kevy-embedded::Store::spop`
//! already did. These tests are red against verb-logging.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use kevy_testnet::free_port;

fn req(parts: &[&[u8]]) -> Vec<u8> {
    let mut v = format!("*{}\r\n", parts.len()).into_bytes();
    for p in parts {
        v.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
        v.extend_from_slice(p);
        v.extend_from_slice(b"\r\n");
    }
    v
}

fn read_crlf_line(s: &mut TcpStream) -> Vec<u8> {
    let mut line = Vec::new();
    let mut b = [0u8; 1];
    loop {
        s.read_exact(&mut b).unwrap();
        line.push(b[0]);
        if line.ends_with(b"\r\n") {
            line.truncate(line.len() - 2);
            return line;
        }
    }
}

/// Minimal RESP2 reply reader — just the shapes these tests receive.
#[derive(Debug, PartialEq)]
enum Resp {
    Simple(Vec<u8>),
    Bulk(Option<Vec<u8>>),
    Int(i64),
    Array(Vec<Resp>),
}

fn read_value(s: &mut TcpStream) -> Resp {
    let line = read_crlf_line(s);
    let (tag, rest) = (line[0], &line[1..]);
    match tag {
        b'+' | b'-' => Resp::Simple(line),
        b':' => Resp::Int(std::str::from_utf8(rest).unwrap().parse().unwrap()),
        b'$' => {
            let n: i64 = std::str::from_utf8(rest).unwrap().parse().unwrap();
            if n < 0 {
                return Resp::Bulk(None);
            }
            let mut payload = vec![0u8; n as usize + 2];
            s.read_exact(&mut payload).unwrap();
            payload.truncate(n as usize);
            Resp::Bulk(Some(payload))
        }
        b'*' => {
            let n: i64 = std::str::from_utf8(rest).unwrap().parse().unwrap();
            let items = (0..n.max(0)).map(|_| read_value(s)).collect();
            Resp::Array(items)
        }
        other => panic!("unexpected RESP tag {other:?}"),
    }
}

fn cmd(s: &mut TcpStream, parts: &[&[u8]]) -> Resp {
    s.write_all(&req(parts)).unwrap();
    read_value(s)
}

/// Sorted member list from an SMEMBERS reply.
fn members(reply: Resp) -> Vec<Vec<u8>> {
    let Resp::Array(items) = reply else {
        panic!("SMEMBERS must reply an array, got {reply:?}");
    };
    let mut out: Vec<Vec<u8>> = items
        .into_iter()
        .map(|it| match it {
            Resp::Bulk(Some(b)) => b,
            other => panic!("non-bulk member {other:?}"),
        })
        .collect();
    out.sort();
    out
}

/// Run a 1-shard runtime (AOF on — the builder default) on `port` over
/// `dir`, hand the port to `body`, then stop and join.
fn with_runtime(port: u16, dir: &std::path::Path, body: impl FnOnce(u16)) {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_t = stop.clone();
    let dir = dir.to_path_buf();
    let handle = std::thread::spawn(move || {
        let rt = kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(1))
            .bind([127, 0, 0, 1], port)
            .shards(1)
            .with_data_dir(dir);
        rt.run(stop_t).unwrap();
    });
    let mut up = false;
    for _ in 0..400 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            up = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(up, "runtime did not start on {port}");
    body(port);
    stop.store(true, Ordering::Relaxed);
    let _ = handle.join();
}

/// The AOF half of the contract: after a storm of SPOPs, a restart must
/// replay to EXACTLY the surviving members — because the log carries
/// `SREM key <popped…>`, never a `SPOP` frame to re-randomize. Also
/// pins the no-leak rule: the SET issued right after a SPOP still
/// reaches the AOF as itself (the override dies with its own command),
/// and an empty-set SPOP leaves no frame at all.
#[test]
fn spop_effect_survives_aof_restart() {
    let dir = kevy_tmpdir::TmpDir::new("spop-aof");
    let port = free_port();

    let mut popped: Vec<Vec<u8>> = Vec::new();
    let mut survivors: Vec<Vec<u8>> = Vec::new();
    with_runtime(port, dir.path(), |p| {
        let mut c = TcpStream::connect(("127.0.0.1", p)).unwrap();
        let all: Vec<Vec<u8>> = (0..50).map(|i| format!("m{i:02}").into_bytes()).collect();
        let mut argv: Vec<&[u8]> = vec![b"SADD", b"s"];
        argv.extend(all.iter().map(Vec::as_slice));
        assert_eq!(cmd(&mut c, &argv), Resp::Int(50));

        // 10 single-member pops (bare reply form).
        for _ in 0..10 {
            match cmd(&mut c, &[b"SPOP", b"s"]) {
                Resp::Bulk(Some(m)) => popped.push(m),
                other => panic!("SPOP: {other:?}"),
            }
        }
        // One count-form pop.
        match cmd(&mut c, &[b"SPOP", b"s", b"5"]) {
            Resp::Array(items) => {
                assert_eq!(items.len(), 5);
                for it in items {
                    let Resp::Bulk(Some(m)) = it else { panic!("non-bulk pop") };
                    popped.push(m);
                }
            }
            other => panic!("SPOP count: {other:?}"),
        }
        // Empty-set pops — the Suppress path (must log NOTHING).
        assert_eq!(cmd(&mut c, &[b"SPOP", b"nosuchset"]), Resp::Bulk(None));
        assert_eq!(cmd(&mut c, &[b"SPOP", b"nosuchset", b"3"]), Resp::Array(vec![]));
        // The write right after a SPOP must record as ITSELF — a leaked
        // override here would swallow or rewrite it.
        assert_eq!(
            cmd(&mut c, &[b"SET", b"marker", b"fence-value"]),
            Resp::Simple(b"+OK".to_vec())
        );
        survivors = members(cmd(&mut c, &[b"SMEMBERS", b"s"]));
        assert_eq!(survivors.len(), 35);
    });

    // The log itself: effect frames only. No `SPOP` verb may reach disk;
    // the pops are there as SREM; the post-SPOP SET is there verbatim.
    let aof = std::fs::read(dir.path().join("aof-0.aof")).unwrap();
    assert!(
        !aof.windows(4).any(|w| w.eq_ignore_ascii_case(b"SPOP")),
        "a SPOP frame reached the AOF — replay will re-randomize"
    );
    assert!(aof.windows(4).any(|w| w == b"SREM"), "no SREM effect frame in the AOF");
    assert!(aof.windows(6).any(|w| w == b"marker"), "post-SPOP SET lost from the AOF");
    assert!(
        !aof.windows(9).any(|w| w == b"nosuchset"),
        "an empty-set SPOP left a frame in the AOF"
    );

    // Restart on the same dir: the replay must reproduce the EXACT set.
    let port2 = free_port();
    with_runtime(port2, dir.path(), |p| {
        let mut c = TcpStream::connect(("127.0.0.1", p)).unwrap();
        let replayed = members(cmd(&mut c, &[b"SMEMBERS", b"s"]));
        assert_eq!(
            replayed, survivors,
            "AOF replay produced a different surviving set — SPOP verb replayed?"
        );
        for m in &popped {
            assert_eq!(
                cmd(&mut c, &[b"SISMEMBER", b"s", m]),
                Resp::Int(0),
                "popped member {:?} resurrected by replay",
                String::from_utf8_lossy(m)
            );
        }
        assert_eq!(cmd(&mut c, &[b"GET", b"marker"]), Resp::Bulk(Some(b"fence-value".to_vec())));
    });
}
