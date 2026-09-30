//! `COPY … REPLACE` and `RENAME` place a value over a key that may
//! already hold one, of the same type or of another. What they record
//! must rebuild the destination as the client saw it: after a restart
//! from the AOF, on a replica, after an AOF rewrite, and from a snapshot.
//! Each case runs with both keys on one shard and with the two keys on
//! two shards, where the destination's shard records the placed value.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::common;
use super::common::node::Node;
use kevy_tmpdir::TmpDir;

/// What the destination `D` and the source `S` hold before the value is
/// placed, the type `D` then has, and the reads that show `D`.
struct Case {
    name: &'static str,
    setup: &'static [&'static str],
    ty: &'static str,
    reads: &'static [&'static str],
}

const CASES: &[Case] = &[
    Case {
        name: "a hash over a hash with other fields",
        setup: &["HSET D a 1", "HSET S b 2"],
        ty: "hash",
        reads: &["HGETALL D"],
    },
    Case {
        name: "a list over a hash",
        setup: &["HSET D a 1", "RPUSH S x y"],
        ty: "list",
        reads: &["LRANGE D 0 -1"],
    },
    Case {
        name: "a sorted set over a string",
        setup: &["SET D old", "ZADD S 1 m"],
        ty: "zset",
        reads: &["ZRANGE D 0 -1 WITHSCORES"],
    },
    Case {
        name: "a set over a set",
        setup: &["SADD D x", "SADD S y"],
        ty: "set",
        reads: &["SMEMBERS D"],
    },
    Case {
        name: "a list over a list",
        setup: &["RPUSH D a", "RPUSH S b"],
        ty: "list",
        reads: &["LRANGE D 0 -1"],
    },
    Case {
        name: "a stream over a hash",
        setup: &["HSET D a 1", "XADD S 1-1 f v"],
        ty: "stream",
        reads: &["XRANGE D - +"],
    },
    Case {
        name: "a hash over a list",
        setup: &["RPUSH D a", "HSET S b 2"],
        ty: "hash",
        reads: &["HGETALL D"],
    },
    Case {
        name: "a string without a deadline over a hash with one",
        setup: &["HSET D a 1", "EXPIRE D 100000", "SET S v"],
        ty: "string",
        reads: &["GET D", "TTL D"],
    },
];

const PLACES: &[&str] = &["COPY S D REPLACE", "RENAME S D"];

fn argv(line: &str, run: &Run) -> Vec<String> {
    let key = |a: &str| match a {
        "S" => run.src.clone(),
        "D" => run.dst.clone(),
        a => a.to_string(),
    };
    line.split(' ').map(key).collect()
}

/// One case placed one way, with its two keys.
struct Run {
    label: String,
    case: &'static Case,
    place: &'static str,
    src: String,
    dst: String,
}

/// Every case placed every way, the keys on one shard and on two.
fn runs(nshards: usize) -> Vec<Run> {
    let shard = |t: char| {
        let tag = format!("{{{t}}}");
        kevy_rt::shard_of_key(tag.as_bytes(), nshards, kevy_persist::Routing::KevyHash)
    };
    let away = ('b'..='z').find(|t| shard(*t) != shard('a')).unwrap_or('a');
    let mut out = Vec::new();
    for (i, case) in CASES.iter().enumerate() {
        for (j, place) in PLACES.iter().enumerate() {
            for tag in ['a', away] {
                let verb = place.split(' ').next().unwrap_or_default();
                let apart = if tag == 'a' { "one shard" } else { "two shards" };
                out.push(Run {
                    label: format!("{} by {verb} on {apart}", case.name),
                    case,
                    place,
                    src: format!("{{a}}s{i}{j}{tag}"),
                    dst: format!("{{{tag}}}d{i}{j}"),
                });
            }
        }
    }
    out
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).replace("\r\n", " ")
}

/// Set every run up and place its value, checking the destination's type.
fn place_all(w: &mut common::Wire, runs: &[Run]) {
    for run in runs {
        for line in run.case.setup {
            w.call(&argv(line, run));
        }
        let placed = w.call(&argv(run.place, run));
        assert!(placed == b":1\r\n" || placed == b"+OK\r\n", "{}: {}", run.label, text(&placed));
        let ty = w.call(&argv("TYPE D", run));
        assert_eq!(text(&ty), format!("+{} ", run.case.ty), "{}", run.label);
    }
}

/// Every run's reads of its destination, as the node answers them.
fn read_all(w: &mut common::Wire, runs: &[Run]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for run in runs {
        let mut got = String::new();
        for line in ["TYPE D"].iter().chain(run.case.reads) {
            got += &text(&w.call(&argv(line, run)));
        }
        out.push((run.label.clone(), got));
    }
    out
}

/// The runs whose destination differs from what the client saw.
fn differ(seen: &[(String, String)], now: &[(String, String)]) -> Vec<String> {
    seen.iter()
        .zip(now)
        .filter(|(a, b)| a.1 != b.1)
        .map(|(a, b)| format!("{}: saw {:?}, now {:?}", a.0, a.1, b.1))
        .collect()
}

#[test]
fn a_placed_value_is_rebuilt_by_a_restart_and_on_a_replica() {
    const NSHARDS: usize = 4;
    let runs = runs(NSHARDS);
    assert!(runs.iter().any(|r| r.label.ends_with("two shards")), "no pair is split");
    let (primary_dir, replica_dir) = (TmpDir::new("placed-primary"), TmpDir::new("placed-replica"));
    let primary = Node::primary(NSHARDS, &primary_dir);
    let replica = Node::replica(&primary, NSHARDS, &replica_dir);
    place_all(&mut primary.wire(), &runs);
    let seen = read_all(&mut primary.wire(), &runs);
    let started = std::time::Instant::now();
    let mut on_replica = differ(&seen, &read_all(&mut replica.wire(), &runs));
    while !on_replica.is_empty() && started.elapsed() < common::BUDGET {
        std::thread::sleep(std::time::Duration::from_millis(25));
        on_replica = differ(&seen, &read_all(&mut replica.wire(), &runs));
    }
    replica.stop();
    primary.stop();
    let restarted = Node::primary(NSHARDS, &primary_dir);
    let after_restart = differ(&seen, &read_all(&mut restarted.wire(), &runs));
    restarted.stop();
    assert!(
        after_restart.is_empty() && on_replica.is_empty(),
        "after a restart: {after_restart:#?}\non the replica: {on_replica:#?}"
    );
}

#[test]
fn a_placed_value_is_rebuilt_by_a_rewritten_log() {
    let runs = runs(1);
    let dir = TmpDir::new("placed-rewrite");
    let primary = Node::primary(1, &dir);
    let mut w = primary.wire();
    place_all(&mut w, &runs);
    let seen = read_all(&mut w, &runs);
    assert_eq!(w.call(&["BGREWRITEAOF"]), b"+OK\r\n");
    common::until("the rewrite to finish", || {
        let info = text(&w.call(&["INFO", "persistence"]));
        info.contains("aof_rewrites_total:1") && info.contains("aof_rewrite_in_progress:0")
    });
    primary.stop();
    let restarted = Node::primary(1, &dir);
    let after = differ(&seen, &read_all(&mut restarted.wire(), &runs));
    restarted.stop();
    assert!(after.is_empty(), "after a rewrite and a restart: {after:#?}");
}

/// A server with no AOF on `dir`, stopped by the returned flag.
fn snapshot_only(dir: &TmpDir) -> (u16, Arc<AtomicBool>, std::thread::JoinHandle<()>) {
    const NSHARDS: usize = 4;
    let port = kevy_testnet::free_port();
    let stop = Arc::new(AtomicBool::new(false));
    let (st, d) = (stop.clone(), dir.path().to_path_buf());
    let handle = std::thread::spawn(move || {
        kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(NSHARDS))
            .bind([127, 0, 0, 1], port)
            .shards(NSHARDS)
            .with_data_dir(d)
            .with_aof(false)
            .run(st)
            .unwrap();
    });
    kevy_testnet::assert_listening(port, "the snapshot-only server");
    (port, stop, handle)
}

fn wire(port: u16) -> common::Wire {
    let s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(std::time::Duration::from_secs(20))).unwrap();
    common::Wire::new(s)
}

#[test]
fn a_placed_value_is_rebuilt_from_a_snapshot() {
    let runs = runs(4);
    let dir = TmpDir::new("placed-snapshot");
    let (port, stop, handle) = snapshot_only(&dir);
    let mut w = wire(port);
    place_all(&mut w, &runs);
    let seen = read_all(&mut w, &runs);
    assert_eq!(w.call(&["SAVE"]), b"+OK\r\n");
    stop.store(true, Ordering::SeqCst);
    let _ = std::net::TcpStream::connect(("127.0.0.1", port));
    handle.join().unwrap();
    let (port, stop, handle) = snapshot_only(&dir);
    let after = differ(&seen, &read_all(&mut wire(port), &runs));
    stop.store(true, Ordering::SeqCst);
    let _ = std::net::TcpStream::connect(("127.0.0.1", port));
    handle.join().unwrap();
    assert!(after.is_empty(), "after a snapshot and a restart: {after:#?}");
}
