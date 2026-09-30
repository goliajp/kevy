//! The index, view and table catalog is state the primary owns like any
//! key: it survives a restart from the log, a save and a log rewrite,
//! and it reaches a replica on the stream and in a full sync. A replica
//! has only what its primary has.

#[path = "common/mod.rs"]
mod common;

use std::path::Path;

use common::Wire;
use common::node::Node;
use kevy_tmpdir::TmpDir;

const READONLY: &[u8] = b"-READONLY You can't write against a read only replica.\r\n";

fn call(c: &mut Wire, cmd: &str) -> Vec<u8> {
    c.call(&cmd.split(' ').collect::<Vec<_>>())
}

fn ok(c: &mut Wire, cmd: &str) {
    assert_eq!(call(c, cmd), b"+OK\r\n", "{cmd}");
}

/// A catalog of every kind: a local index, a global one with an explicit
/// split, a view over the local one, a table (which compiles its own
/// index), and an index created and dropped again.
fn declare(c: &mut Wire) {
    for i in 0..20 {
        assert_eq!(call(c, &format!("HSET user:{i} age {i}")), b":1\r\n");
    }
    ok(c, "IDX.CREATE age ON PREFIX user: FIELD age TYPE i64 KIND range");
    ok(c, "IDX.CREATE g ON PREFIX user: FIELD age TYPE i64 KIND range PARTITION global SPLIT 10");
    ok(c, "IDX.CREATE gone ON PREFIX user: FIELD age TYPE i64 KIND range");
    assert_eq!(call(c, "IDX.DROP gone"), b":1\r\n");
    ok(c, "VIEW.CREATE adults QUERY age RANGE 18 200 ORDER BY age");
    ok(
        c,
        "TABLE.DECLARE users PREFIX users: PK id COLUMN id i64 COLUMN email str INDEX email unique",
    );
}

/// The names a `*.LIST` reply holds, in order.
fn names(reply: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(reply);
    let lines: Vec<&str> = text.split("\r\n").collect();
    lines
        .windows(3)
        .filter(|w| w[0] == "name" && w[1].starts_with('$'))
        .map(|w| w[2].to_string())
        .collect()
}

/// The split points `IDX.DESCRIBE` reports for a global index.
fn splits(c: &mut Wire, name: &str) -> Vec<String> {
    let d = String::from_utf8_lossy(&call(c, &format!("IDX.DESCRIBE {name}"))).into_owned();
    let Some(at) = d.find("$6\r\nglobal\r\n*") else { return Vec::new() };
    let lines: Vec<&str> = d[at..].split("\r\n").collect();
    let n: usize = lines[2][1..].parse().unwrap();
    (0..n).map(|i| lines[4 + 2 * i].to_string()).collect()
}

/// What a node's catalog holds and answers: the names of each kind, the
/// global index's splits, and a query through the index and the view.
fn catalog(node: &Node) -> Vec<String> {
    let mut c = node.wire();
    let mut seen = Vec::new();
    for list in ["IDX.LIST", "VIEW.LIST", "TABLE.LIST"] {
        seen.push(format!("{list}: {:?}", names(&call(&mut c, list))));
    }
    seen.push(format!("g splits: {:?}", splits(&mut c, "g")));
    for query in ["IDX.QUERY age RANGE 15 17", "VIEW.QUERY adults"] {
        seen.push(format!("{query}: {}", String::from_utf8_lossy(&call(&mut c, query))));
    }
    seen
}

/// The catalog `node` settles on, polled while backfills run.
fn settled(node: &Node, want: &[String]) -> Vec<String> {
    let started = std::time::Instant::now();
    let mut got = catalog(node);
    while got != want && started.elapsed() < common::BUDGET {
        std::thread::sleep(std::time::Duration::from_millis(25));
        got = catalog(node);
    }
    got
}

/// The catalog `node` holds once two reads in a row agree and no index
/// is still building.
fn steady(node: &Node) -> Vec<String> {
    let mut last = catalog(node);
    common::until("the catalog to hold still", || {
        let now = catalog(node);
        let steady = now == last && !now.iter().any(|s| s.contains("BUILDING"));
        last = now;
        steady
    });
    last
}

fn declared_on(primary: &Node) -> Vec<String> {
    declare(&mut primary.wire());
    let want = steady(primary);
    assert!(want[0].contains("\"age\"") && want[0].contains("\"g\""), "{want:?}");
    assert!(!want[0].contains("gone"), "{want:?}");
    want
}

#[test]
fn a_replica_follows_the_primarys_catalog_on_the_stream() {
    let (pdir, rdir) = (TmpDir::new("catalog-primary"), TmpDir::new("catalog-replica"));
    let primary = Node::primary(2, &pdir);
    let replica = Node::replica(&primary, 2, &rdir);
    // the replica has caught up with an empty catalog before any DDL
    assert_eq!(call(&mut primary.wire(), "SET probe 1"), b"+OK\r\n");
    let mut r = replica.wire();
    common::until("the replica to catch up", || call(&mut r, "GET probe") == b"$1\r\n1\r\n");
    let want = declared_on(&primary);
    assert_eq!(settled(&replica, &want), want);
    // and it declares nothing of its own
    let mine = "IDX.CREATE mine ON PREFIX user: FIELD age TYPE i64 KIND range";
    assert_eq!(call(&mut r, mine), READONLY);
    assert_eq!(call(&mut r, "IDX.DROP age"), READONLY);
    assert_eq!(call(&mut primary.wire(), "IDX.DROP age"), b":1\r\n");
    let want = steady(&primary);
    assert!(!want[0].contains("\"age\""), "{want:?}");
    assert_eq!(settled(&replica, &want), want);
    replica.stop();
    primary.stop();
}

#[test]
fn a_replica_that_joins_later_gets_the_catalog_in_its_full_sync() {
    let (pdir, rdir) = (TmpDir::new("catalog-primary-late"), TmpDir::new("catalog-replica-late"));
    let primary = Node::primary(2, &pdir);
    let want = declared_on(&primary);
    let replica = Node::replica(&primary, 2, &rdir);
    assert_eq!(settled(&replica, &want), want);
    replica.stop();
    primary.stop();
}

/// Restart a two-shard primary after `then` and expect the same catalog.
fn survives(label: &str, then: impl FnOnce(&Node, &Path)) {
    let dir = TmpDir::new(label);
    let primary = Node::primary(2, &dir);
    let want = declared_on(&primary);
    then(&primary, dir.path());
    primary.stop();
    let restarted = Node::primary(2, &dir);
    assert_eq!(settled(&restarted, &want), want, "{label}");
    restarted.stop();
}

/// Whether every shard's `aof-<i>.aof` holds `needle`.
fn every_log_holds(dir: &Path, needle: &[u8]) -> bool {
    (0..2).all(|i| {
        std::fs::read(dir.join(format!("aof-{i}.aof")))
            .is_ok_and(|bytes| bytes.windows(needle.len()).any(|w| w == needle))
    })
}

#[test]
fn the_catalog_survives_a_restart_from_the_log() {
    survives("catalog-restart-log", |_, _| {});
}

#[test]
fn the_catalog_survives_a_save_that_resets_the_log() {
    survives("catalog-restart-save", |p, dir| {
        assert!(call(&mut p.wire(), "BGSAVE").starts_with(b"+"));
        common::until("both shards to save", || {
            (0..2).all(|i| dir.join(format!("dump-{i}.rdb")).exists())
        });
    });
}

/// A rewrite regenerates each shard's log from its keyspace; every
/// rewritten log carries the catalog, including the shard that recorded
/// none of the DDL.
#[test]
fn the_catalog_survives_a_log_rewrite() {
    survives("catalog-restart-rewrite", |p, dir| {
        assert!(call(&mut p.wire(), "BGREWRITEAOF").starts_with(b"+"));
        common::until("every shard's log to be rewritten with the catalog", || {
            every_log_holds(dir, b"XINTERNAL.CATALOG")
        });
    });
}

/// A reshard replaces every shard's files with fresh snapshots and empty
/// logs; the catalog rides in those snapshots, so it outlives the reshard
/// and the start after it.
#[test]
fn the_catalog_survives_a_reshard() {
    let dir = TmpDir::new("catalog-reshard");
    let primary = Node::primary(2, &dir);
    let want = declared_on(&primary);
    primary.stop();
    for label in ["resharded", "restarted after the reshard"] {
        let node = Node::primary(4, &dir);
        assert_eq!(settled(&node, &want), want, "{label}");
        node.stop();
    }
}

/// A promoted replica holds its primary's catalog and goes on changing
/// it as a primary.
#[test]
fn a_promoted_replica_goes_on_from_its_primarys_catalog() {
    let (pdir, rdir) = (TmpDir::new("catalog-primary-promote"), TmpDir::new("catalog-promoted"));
    let primary = Node::primary(2, &pdir);
    let replica = Node::replica(&primary, 2, &rdir);
    let want = declared_on(&primary);
    assert_eq!(settled(&replica, &want), want);
    primary.stop();
    let mut r = replica.wire();
    ok(&mut r, "REPLICAOF NO ONE");
    ok(&mut r, "IDX.CREATE later ON PREFIX user: FIELD age TYPE i64 KIND range");
    let now = steady(&replica);
    assert!(now[0].contains("\"later\"") && now[0].contains("\"age\""), "{now:?}");
    assert_eq!(now[1..], want[1..]);
    replica.stop();
}

/// A directory 6.4.0 wrote (its catalog in sidecar files) opens with its
/// catalog: read once, recorded in the log, and the sidecars removed, so
/// the next start takes the catalog from the log alone.
#[test]
fn a_6_4_directory_brings_its_catalog_into_the_log() {
    let dir = TmpDir::new("catalog-from-6.4");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/data-dir-6.4.0");
    for entry in std::fs::read_dir(&fixture).unwrap() {
        let from = entry.unwrap().path();
        let name = from.file_name().unwrap().to_str().unwrap().trim_end_matches(".in").to_string();
        std::fs::copy(&from, dir.path().join(name)).unwrap();
    }
    let sidecars = ["index-catalog.meta", "view-catalog.meta", "table-catalog.meta"];
    let node = Node::primary(2, &dir);
    let want = steady(&node);
    assert_eq!(want[0], r#"IDX.LIST: ["user_age", "user_plan", "users.email"]"#);
    assert_eq!(want[1], r#"VIEW.LIST: ["adults"]"#);
    assert_eq!(want[2], r#"TABLE.LIST: ["users"]"#);
    let adults = call(&mut node.wire(), "VIEW.QUERY adults");
    assert_eq!(adults, b"*2\r\n$1\r\n0\r\n*2\r\n$6\r\nuser:1\r\n$2\r\n30\r\n");
    assert!(sidecars.iter().all(|s| !dir.path().join(s).exists()), "the sidecars stayed");
    node.stop();
    let node = Node::primary(2, &dir);
    assert_eq!(steady(&node), want);
    node.stop();
}
