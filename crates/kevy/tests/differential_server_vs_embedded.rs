//! The differential harness: does the embedded facade answer what the server
//! answers?
//!
//! The clone atlas (`bench/CLONE-ATLAS.md`, 2026-08-27) found one pair
//! dominating every other duplication signal in the workspace:
//! `kevy-embedded` against `kevy`, 35 of the top 60 cross-crate pairs and 751
//! shared fingerprints, an order of magnitude past anything else. The matches
//! line up command by command — `dispatch/idx_create.rs` with `cmd_index.rs`,
//! `dispatch/zset.rs` with `cmd_zadd.rs`, `dispatch/view.rs` with
//! `cmd_view.rs` — across roughly 4,077 and 5,336 lines.
//!
//! Shape matching is not proof that two implementations agree, and the atlas
//! says so itself: fingerprints find code that was copied, never two
//! different implementations of one capability. This is the instrument that
//! can settle it, because both sides expose the same observable:
//!
//! * server:   `kevy::KevyCommands::dispatch(&mut Store, argv) -> Vec<u8>`
//! * embedded: `kevy_embedded::Store::dispatch_argv(argv, &mut Vec<u8>)`
//!
//! Both take an argv and produce RESP bytes. So the comparison is
//! byte-for-byte with nothing to interpret, no socket, and no server process.
//!
//! What this decides: whether the duplication is redundancy that could be
//! removed, or two implementations that genuinely differ and must both exist.
//! Divergence is not automatically a bug — the two have different transports
//! and different lifetimes — but every divergence must be *named*, in
//! `EXPECTED`, with the reason. An unnamed one fails the test.

use std::collections::BTreeSet;

/// One in-process server dispatcher per thread, so per-state caches persist
/// across a corpus the way they do for a real connection.
fn server_reply(store: &mut kevy_store::Store, argv: &[Vec<u8>]) -> Vec<u8> {
    thread_local! {
        static KEVY: kevy::KevyCommands = kevy::KevyCommands::new();
    }
    let a = kevy::Argv::from(argv.to_vec());
    KEVY.with(|k| k.dispatch(store, &a))
}

fn embedded_reply(store: &kevy_embedded::Store, argv: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    store.dispatch_argv(argv, &mut out);
    out
}

fn argv(cmd: &str) -> Vec<Vec<u8>> {
    cmd.split(' ').map(|s| s.as_bytes().to_vec()).collect()
}

/// The corpus. Ordered and stateful on purpose: a reply that depends on what
/// came before is exactly where two implementations drift apart.
///
/// It leans on the surface the clone atlas flagged — index, zset, view — and
/// on error shapes, because an error string is where a second implementation
/// is most likely to have been written rather than shared.
const CORPUS: &[&str] = &[
    // basics
    "PING",
    "SET k v",
    "GET k",
    "EXISTS k",
    "TYPE k",
    "STRLEN k",
    "APPEND k more",
    "GET k",
    "DEL k",
    "GET k",
    "SET n 10",
    "INCR n",
    "INCRBY n 5",
    "DECR n",
    "GET n",
    // wrong types and arities: the error surface
    "LPUSH n x",
    "INCR",
    "SET",
    "GET a b c",
    "EXPIRE k",
    "SUBSCRIBE",
    // hash
    "HSET h f1 v1 f2 v2",
    "HGET h f1",
    "HMGET h f1 f2 missing",
    "HLEN h",
    "HDEL h f1",
    "HGETALL h",
    "HINCRBY h counter 3",
    "HSETNX h f2 nope",
    "HGET h f2",
    // list
    "RPUSH l a b c",
    "LRANGE l 0 -1",
    "LLEN l",
    "LPOP l",
    "LINDEX l 0",
    // set
    "SADD s a b c",
    "SMEMBERS s",
    "SISMEMBER s a",
    "SCARD s",
    "SREM s a",
    // zset — dispatch/zset.rs against cmd_zadd.rs
    "ZADD z 1 one 2 two 3 three",
    "ZSCORE z two",
    "ZCARD z",
    "ZRANGE z 0 -1",
    "ZRANGE z 0 -1 WITHSCORES",
    "ZRANGEBYSCORE z 1 2",
    "ZINCRBY z 5 one",
    "ZSCORE z one",
    "ZRANK z three",
    "ZREM z two",
    "ZCOUNT z 0 10",
    "ZADD z notanumber x",
    // keyspace
    "DBSIZE",
    "EXPIRE n 100",
    "TTL n",
    "PERSIST n",
    "TTL n",
    "RENAME n n2",
    "GET n2",
    // index — dispatch/idx_create.rs against cmd_index.rs
    "IDX.CREATE t SCHEMA name TEXT age NUMERIC",
    "IDX.LIST",
    "IDX.INFO t",
    "IDX.CREATE t SCHEMA name TEXT",
    "IDX.QUERY t *",
    "IDX.DROP t",
    "IDX.INFO t",
    // view — dispatch/view.rs against cmd_view.rs
    "VIEW.LIST",
    "VIEW.INFO nope",
    // unknown: both sides should agree on not knowing
    "TOTALLY.NOT.A.COMMAND",
    "TOTALLY.NOT.A.COMMAND with args",
];

/// Divergences that are correct, each with the reason it is correct.
///
/// Written after the first run, not before: a reason invented ahead of the
/// measurement is a reason invented to make a test pass. Each was checked
/// against the source before being written down.
///
/// Note that three of these four are boundaries of THIS HARNESS rather than
/// differences between the implementations. That distinction matters and is
/// why the reason, not the entry, is the useful part.
const EXPECTED: &[(&str, &str)] = &[
    (
        "SUBSCRIBE",
        "A real difference, and the right one. The embedded facade exposes          subscription as a typed API — ops.rs:439, `pub fn subscribe(&self,          channels) -> Subscription` — rather than as a RESP verb, because an          in-process caller holds the Subscription object and there is no          connection whose state SUBSCRIBE could change. The server answers          with an arity error because it does know the verb.",
    ),
    (
        "IDX.LIST",
        "A boundary of this harness, not of the server. cmd_resolve.rs:189          routes IDX.LIST to Route::Extension, and the bare KevyCommands          dispatcher driven here does not carry the index runtime, so it          reports the verb as unknown. The server does implement it          (cmd_index_query.rs:86). Closing this means driving the server          through its extension route.",
    ),
    (
        "IDX.QUERY t *",
        "Same harness boundary: cmd_resolve.rs:184 routes IDX.QUERY to          Route::Extension. The server implements it in          cmd_index_query.rs:105.",
    ),
    (
        "VIEW.LIST",
        "Same harness boundary: cmd_resolve.rs:191 routes VIEW.LIST to          Route::Extension. The server implements it in          cmd_view_reduce.rs:182.",
    ),
];

fn render(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .replace("\r\n", "\\r\\n")
        .chars()
        .take(160)
        .collect()
}

#[test]
fn embedded_answers_what_the_server_answers() {
    let dir = kevy_tmpdir::TmpDir::new("diff-embedded");
    let mut cfg = kevy_embedded::Config::default();
    cfg = cfg.with_persist(dir.path().to_str().expect("utf8 path"));
    let embedded = kevy_embedded::Store::open(cfg).expect("open embedded store");
    let mut server = kevy_store::Store::new();

    let expected: BTreeSet<&str> = EXPECTED.iter().map(|(c, _)| *c).collect();
    let mut diverged = Vec::new();
    let mut agreed = 0usize;

    for cmd in CORPUS {
        let a = argv(cmd);
        let s = server_reply(&mut server, &a);
        let e = embedded_reply(&embedded, &a);
        if s == e {
            agreed += 1;
        } else {
            diverged.push((*cmd, s, e));
        }
    }

    let unnamed: Vec<_> = diverged
        .iter()
        .filter(|(c, _, _)| !expected.contains(c))
        .collect();

    println!(
        "differential: {} of {} commands agree byte-for-byte; {} diverge \
         ({} named in EXPECTED, {} not)",
        agreed,
        CORPUS.len(),
        diverged.len(),
        diverged.len() - unnamed.len(),
        unnamed.len()
    );
    for (cmd, s, e) in &diverged {
        let tag = if expected.contains(cmd) { "named" } else { "UNNAMED" };
        println!("  [{tag}] {cmd}");
        println!("      server:   {}", render(s));
        println!("      embedded: {}", render(e));
    }

    // A harness that measured nothing must not read as agreement.
    assert!(
        agreed + diverged.len() == CORPUS.len() && !CORPUS.is_empty(),
        "the corpus did not run"
    );

    assert!(
        unnamed.is_empty(),
        "{} command(s) diverge without a stated reason; \
         add each to EXPECTED with why it is correct, or fix the divergence",
        unnamed.len()
    );
}
