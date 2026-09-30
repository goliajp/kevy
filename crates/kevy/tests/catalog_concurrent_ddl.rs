//! Catalog commands that run at the same moment on different shards all
//! take effect: each one's change is in the catalog, in the log it comes
//! back from after a restart, and on a replica. A change is computed from
//! the catalog as it stood, so two computed from the same catalog must not
//! both install, the later dropping the earlier.

#[path = "common/mod.rs"]
mod common;

use std::sync::{Arc, Barrier};

use common::Wire;
use common::node::Node;
use kevy_tmpdir::TmpDir;

const NSHARDS: usize = 4;
/// Rounds each connection sends: two indexes a round, and the catalog
/// holds at most 64.
const ROUNDS: usize = 7;

fn call(c: &mut Wire, cmd: &str) -> Vec<u8> {
    c.call(&cmd.split(' ').collect::<Vec<_>>())
}

/// The names a `*.LIST` reply holds, sorted.
fn names(reply: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(reply);
    let lines: Vec<&str> = text.split("\r\n").collect();
    let mut names: Vec<String> = lines
        .windows(3)
        .filter(|w| w[0] == "name" && w[1].starts_with('$'))
        .map(|w| w[2].to_string())
        .collect();
    names.sort();
    names
}

/// One connection on each shard, through the shard's own listener.
fn one_per_shard(node: &Node) -> Vec<Wire> {
    node.shard_ports.iter().map(|&p| node.wire_on(p)).collect()
}

/// A hashtag whose names shard `shard`'s own listener serves: it answers
/// a name that lives on another shard with `-MOVED`.
fn tag_on(shard: usize) -> String {
    (0..)
        .map(|k| format!("h{k}"))
        .find(|t| {
            let key = format!("{{{t}}}");
            kevy_rt::shard_of_key(key.as_bytes(), NSHARDS, kevy_persist::Routing::Slots) == shard
        })
        .unwrap()
}

/// The name stem connection `conn` uses in `round`, on its own shard.
fn stem(conn: usize, round: usize) -> String {
    format!("{{{}}}c{conn}r{round}", tag_on(conn))
}

/// What each connection declares: an index, a view over it and a table
/// per round, every name its own.
fn ddl(conn: usize, round: usize) -> [String; 3] {
    let tag = stem(conn, round);
    [
        format!("IDX.CREATE i{tag} ON PREFIX p{tag}: FIELD age TYPE i64 KIND range"),
        format!("VIEW.CREATE v{tag} QUERY i{tag} RANGE 0 10 ORDER BY i{tag}"),
        format!(
            "TABLE.DECLARE t{tag} PREFIX t{tag}: PK id COLUMN id i64 COLUMN n i64 INDEX n range"
        ),
    ]
}

/// Every connection sends its catalog commands at once, one per shard;
/// the commands that were refused, with their replies. A view refused
/// for an unknown index is a sign of the loss: its connection created
/// that index just before.
fn declare_at_once(node: &Node) -> Vec<String> {
    let barrier = Arc::new(Barrier::new(NSHARDS));
    let threads: Vec<_> = one_per_shard(node)
        .into_iter()
        .enumerate()
        .map(|(conn, mut c)| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let mut refused = Vec::new();
                for round in 0..ROUNDS {
                    for cmd in ddl(conn, round) {
                        let reply = call(&mut c, &cmd);
                        if reply != b"+OK\r\n" {
                            refused.push(format!("{cmd}: {}", String::from_utf8_lossy(&reply)));
                        }
                    }
                }
                refused
            })
        })
        .collect();
    threads.into_iter().flat_map(|t| t.join().unwrap()).collect()
}

/// The index, view and table names every connection declared.
fn declared() -> [Vec<String>; 3] {
    let mut want: [Vec<String>; 3] = Default::default();
    for conn in 0..NSHARDS {
        for round in 0..ROUNDS {
            let tag = stem(conn, round);
            want[0].push(format!("i{tag}"));
            want[0].push(format!("t{tag}.n"));
            want[1].push(format!("v{tag}"));
            want[2].push(format!("t{tag}"));
        }
    }
    want.iter_mut().for_each(|w| w.sort());
    want
}

fn catalog(node: &Node) -> [Vec<String>; 3] {
    let mut c = node.wire();
    ["IDX.LIST", "VIEW.LIST", "TABLE.LIST"].map(|list| names(&call(&mut c, list)))
}

/// Missing names per kind, for a failure message that says what was lost.
fn missing(got: &[Vec<String>; 3], want: &[Vec<String>; 3]) -> Vec<String> {
    (0..3).flat_map(|k| want[k].iter().filter(move |n| !got[k].contains(n)).cloned()).collect()
}

fn settled(node: &Node, want: &[Vec<String>; 3]) -> [Vec<String>; 3] {
    let started = std::time::Instant::now();
    let mut got = catalog(node);
    while &got != want && started.elapsed() < common::BUDGET {
        std::thread::sleep(std::time::Duration::from_millis(25));
        got = catalog(node);
    }
    got
}

#[test]
fn catalog_commands_at_once_on_every_shard_all_take_effect() {
    let (pdir, rdir) = (TmpDir::new("ddl-race-primary"), TmpDir::new("ddl-race-replica"));
    let primary = Node::primary_on_shard_ports(NSHARDS, &pdir);
    let replica = Node::replica(&primary, NSHARDS, &rdir);
    let refused = declare_at_once(&primary);
    let want = declared();
    let got = catalog(&primary);
    let lost = missing(&got, &want);
    assert!(
        got == want && refused.is_empty(),
        "the primary lost {} of {}: {lost:?}; {} commands refused, first {:?}",
        lost.len(),
        want.iter().map(Vec::len).sum::<usize>(),
        refused.len(),
        refused.first()
    );
    let got = settled(&replica, &want);
    assert!(got == want, "the replica lost {:?}", missing(&got, &want));
    replica.stop();
    primary.stop();
    let restarted = Node::primary_on_shard_ports(NSHARDS, &pdir);
    let got = settled(&restarted, &want);
    assert!(got == want, "the restart lost {:?}", missing(&got, &want));
    restarted.stop();
}
