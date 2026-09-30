//! A blocking command that parks and is later served by a write changes
//! the keyspace like any other write, so what it changed must be in the
//! AOF and reach a replica: after a restart, and on the replica, the
//! served pop, move or group read is still there.
//!
//! The serve runs on one of two paths: the in-shard wake, when the key
//! lives on the waiting connection's own shard, and the cross-shard
//! serve, when it lives on another. The four-shard test places every key
//! away from the waiter's shard, so only the cross-shard path can serve
//! it.

#[path = "common/mod.rs"]
mod common;

use common::Wire;
use common::node::Node;
use kevy_tmpdir::TmpDir;

type Cmd = &'static [&'static [u8]];

/// A blocking command and everything around it: what sets it up, the
/// write that serves it, what the waiter is answered, and what the
/// keyspace must say afterwards. Every key carries the hashtag `{T}`,
/// which a test sets to place the keys on a shard of its choosing.
struct Case {
    name: &'static str,
    setup: &'static [Cmd],
    block: Cmd,
    serve: Cmd,
    answer: &'static [u8],
    checks: &'static [(Cmd, &'static [u8])],
}

const CASES: &[Case] = &[
    Case {
        name: "BLPOP",
        setup: &[],
        block: &[b"BLPOP", b"{T}q", b"0"],
        serve: &[b"RPUSH", b"{T}q", b"x", b"y"],
        answer: b"*2\r\n$4\r\n{T}q\r\n$1\r\nx\r\n",
        checks: &[(&[b"LRANGE", b"{T}q", b"0", b"-1"], b"*1\r\n$1\r\ny\r\n")],
    },
    Case {
        name: "BZPOPMIN",
        setup: &[],
        block: &[b"BZPOPMIN", b"{T}z", b"0"],
        serve: &[b"ZADD", b"{T}z", b"1", b"a", b"2", b"b"],
        answer: b"*3\r\n$4\r\n{T}z\r\n$1\r\na\r\n$1\r\n1\r\n",
        checks: &[(&[b"ZRANGE", b"{T}z", b"0", b"-1"], b"*1\r\n$1\r\nb\r\n")],
    },
    Case {
        name: "BRPOPLPUSH",
        setup: &[],
        block: &[b"BRPOPLPUSH", b"{T}src", b"{T}dst", b"0"],
        serve: &[b"RPUSH", b"{T}src", b"x", b"y"],
        answer: b"$1\r\ny\r\n",
        checks: &[
            (&[b"LRANGE", b"{T}src", b"0", b"-1"], b"*1\r\n$1\r\nx\r\n"),
            (&[b"LRANGE", b"{T}dst", b"0", b"-1"], b"*1\r\n$1\r\ny\r\n"),
        ],
    },
    Case {
        name: "XREADGROUP",
        setup: &[&[b"XGROUP", b"CREATE", b"{T}s", b"g", b"$", b"MKSTREAM"]],
        block: &[b"XREADGROUP", b"GROUP", b"g", b"c", b"BLOCK", b"0", b"STREAMS", b"{T}s", b">"],
        serve: &[b"XADD", b"{T}s", b"1-1", b"f", b"v"],
        answer: b"*1\r\n*2\r\n$4\r\n{T}s\r\n*1\r\n*2\r\n$3\r\n1-1\r\n*2\r\n$1\r\nf\r\n$1\r\nv\r\n",
        // the read left one entry pending for `c`
        checks: &[(
            &[b"XPENDING", b"{T}s", b"g"],
            b"*4\r\n:1\r\n$3\r\n1-1\r\n$3\r\n1-1\r\n*1\r\n*2\r\n$1\r\nc\r\n$1\r\n1\r\n",
        )],
    },
];

/// `bytes` with every `{T}` set to `{tag}`: one character for one, so
/// every length in an expected reply still holds.
fn tagged(bytes: &[u8], tag: u8) -> Vec<u8> {
    let mut out = bytes.to_vec();
    for i in 0..out.len().saturating_sub(2) {
        if out[i..i + 3] == *b"{T}" {
            out[i + 1] = tag;
        }
    }
    out
}

fn tagged_cmd(cmd: Cmd, tag: u8) -> Vec<Vec<u8>> {
    cmd.iter().map(|a| tagged(a, tag)).collect()
}

fn blocked_clients(c: &mut Wire) -> usize {
    let info = c.call(&[b"INFO" as &[u8], b"clients"]);
    let text = String::from_utf8_lossy(&info);
    text.lines()
        .find_map(|l| l.strip_prefix("blocked_clients:"))
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(0)
}

/// Park `waiter` on the case's blocking command, serve it with a write
/// from `writer`, and check the waiter's answer.
fn park_and_serve(case: &Case, tag: u8, waiter: &mut Wire, writer: &mut Wire) {
    for cmd in case.setup {
        writer.call(&tagged_cmd(cmd, tag));
    }
    // the gauge is published per shard, so a served waiter can still be
    // counted for a moment: wait for none, then for this one
    common::until("no waiter left", || blocked_clients(writer) == 0);
    waiter.send(&tagged_cmd(case.block, tag));
    common::until(&format!("{} to park", case.name), || blocked_clients(writer) == 1);
    writer.call(&tagged_cmd(case.serve, tag));
    assert_eq!(waiter.reply(), tagged(case.answer, tag), "{}: the waiter's answer", case.name);
}

/// What does not hold on `node` for `cases`, one line per check.
fn wrong_on(node: &Node, tag: u8, cases: &[&Case]) -> Vec<String> {
    let mut c = node.wire();
    let mut wrong = Vec::new();
    for case in cases {
        for (cmd, want) in case.checks {
            let got = c.call(&tagged_cmd(cmd, tag));
            if got != tagged(want, tag) {
                let got = String::from_utf8_lossy(&got).replace("\r\n", " ");
                wrong.push(format!("{}: {got}", case.name));
            }
        }
    }
    wrong
}

/// One shard: every key lives on the waiter's shard, so the in-shard
/// wake serves each command.
#[test]
fn a_served_blocking_command_survives_a_restart_and_reaches_a_replica() {
    let cases: Vec<&Case> = CASES.iter().collect();
    let (primary_dir, replica_dir) = (TmpDir::new("serve-primary"), TmpDir::new("serve-replica"));
    let primary = Node::primary(1, &primary_dir);
    let replica = Node::replica(&primary, 1, &replica_dir);
    let mut writer = primary.wire();
    for case in &cases {
        park_and_serve(case, b'a', &mut primary.wire(), &mut writer);
    }
    let on_primary = wrong_on(&primary, b'a', &cases);
    assert!(on_primary.is_empty(), "on the primary, these do not hold: {on_primary:#?}");
    let started = std::time::Instant::now();
    while !wrong_on(&replica, b'a', &cases).is_empty() && started.elapsed() < common::BUDGET {
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let on_replica = wrong_on(&replica, b'a', &cases);
    replica.stop();
    primary.stop();
    let restarted = Node::primary(1, &primary_dir);
    let after_restart = wrong_on(&restarted, b'a', &cases);
    restarted.stop();
    assert!(after_restart.is_empty(), "after a restart, these do not hold: {after_restart:#?}");
    assert!(on_replica.is_empty(), "on the replica, these do not hold: {on_replica:#?}");
}

/// A connection to `node` and a hashtag whose keys live on another shard
/// than the connection's. A connection's id is `shard + 1 + k * nshards`,
/// so its shard is known.
fn waiter_away_from_its_keys(node: &Node, nshards: usize) -> (Wire, u8) {
    let mut waiter = node.wire();
    let id = waiter.call(&[b"CLIENT" as &[u8], b"ID"]);
    let id: u64 = std::str::from_utf8(&id[1..id.len() - 2]).unwrap().parse().unwrap();
    let waiter_shard = (id - 1) as usize % nshards;
    let shard = |tag: u8| {
        kevy_rt::shard_of_key(&[b'{', tag, b'}'], nshards, kevy_persist::Routing::KevyHash)
    };
    let tag = (b'a'..=b'z').find(|t| shard(*t) != waiter_shard).expect("a key on another shard");
    (waiter, tag)
}

/// Four shards, every key placed away from the waiting connection's
/// shard, so only the cross-shard path can serve it.
#[test]
fn a_cross_shard_serve_survives_a_restart() {
    const NSHARDS: usize = 4;
    let cases: Vec<&Case> = CASES.iter().collect();
    let dir = TmpDir::new("serve-xshard");
    let primary = Node::primary(NSHARDS, &dir);
    let mut writer = primary.wire();
    let (mut waiter, tag) = waiter_away_from_its_keys(&primary, NSHARDS);
    for case in &cases {
        park_and_serve(case, tag, &mut waiter, &mut writer);
    }
    let on_primary = wrong_on(&primary, tag, &cases);
    assert!(on_primary.is_empty(), "on the primary, these do not hold: {on_primary:#?}");
    primary.stop();
    let restarted = Node::primary(NSHARDS, &dir);
    let after_restart = wrong_on(&restarted, tag, &cases);
    restarted.stop();
    assert!(after_restart.is_empty(), "after a restart, these do not hold: {after_restart:#?}");
}

/// A blocking group read on another shard's stream answers at once when
/// the group is missing, as it does on the stream's own shard, and serves
/// at once when the group already has something to read.
#[test]
fn a_cross_shard_group_read_answers_at_once_when_it_can() {
    const NSHARDS: usize = 4;
    let dir = TmpDir::new("serve-xshard-group");
    let primary = Node::primary(NSHARDS, &dir);
    let mut writer = primary.wire();
    let (mut waiter, tag) = waiter_away_from_its_keys(&primary, NSHARDS);
    let read: Cmd =
        &[b"XREADGROUP", b"GROUP", b"g", b"c", b"BLOCK", b"0", b"STREAMS", b"{T}s", b">"];
    let missing = waiter.call(&tagged_cmd(read, tag));
    assert!(missing.starts_with(b"-NOGROUP"), "{}", String::from_utf8_lossy(&missing));
    writer.call(&tagged_cmd(&[b"XGROUP", b"CREATE", b"{T}s", b"g", b"0", b"MKSTREAM"], tag));
    writer.call(&tagged_cmd(&[b"XADD", b"{T}s", b"1-1", b"f", b"v"], tag));
    let answer: &[u8] =
        b"*1\r\n*2\r\n$4\r\n{T}s\r\n*1\r\n*2\r\n$3\r\n1-1\r\n*2\r\n$1\r\nf\r\n$1\r\nv\r\n";
    assert_eq!(waiter.call(&tagged_cmd(read, tag)), tagged(answer, tag));
    primary.stop();
}
