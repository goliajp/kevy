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

/// A replica that follows another primary takes that primary's catalog,
/// an empty one included.
#[test]
fn a_replica_that_follows_a_primary_with_no_catalog_holds_none() {
    let (adir, bdir) = (TmpDir::new("catalog-primary-a"), TmpDir::new("catalog-primary-b"));
    let rdir = TmpDir::new("catalog-replica-switch");
    let a = Node::primary(2, &adir);
    let replica = Node::replica(&a, 2, &rdir);
    let want = declared_on(&a);
    assert_eq!(settled(&replica, &want), want);
    let b = Node::primary(2, &bdir);
    let empty = steady(&b);
    assert!(empty[0].ends_with("[]"), "{empty:?}");
    let upstream = b.replication_base.to_string();
    ok(&mut replica.wire(), &format!("REPLICAOF 127.0.0.1 {upstream}"));
    assert_eq!(settled(&replica, &empty), empty);
    replica.stop();
    a.stop();
    b.stop();
}

/// The catalog's `(lineage, version)` shard 0's snapshot in `dir` carries.
fn saved_at(dir: &Path) -> (Vec<u8>, Vec<u8>) {
    let file = std::fs::File::open(dir.join("dump-0.rdb")).unwrap();
    let mut store = kevy_store::Store::new();
    let image = std::io::BufReader::new(file);
    let aux = kevy_persist::load_snapshot_with_aux(&mut store, image, |_| true).unwrap();
    let aux = aux.expect("the snapshot carries the catalog's frame");
    (aux[1].to_vec(), aux[2].to_vec())
}

/// Save every shard of `node` afresh and read what shard 0 kept.
fn saved(node: &Node, dir: &Path) -> (Vec<u8>, Vec<u8>) {
    // a node listens before its shards have restored, and serves after:
    // the snapshots it may still be reading stay until it answers
    assert_eq!(call(&mut node.wire(), "EXISTS none"), b":0\r\n");
    let dump = |i: usize| dir.join(format!("dump-{i}.rdb"));
    for i in 0..2 {
        drop(std::fs::remove_file(dump(i)));
    }
    assert!(call(&mut node.wire(), "BGSAVE").starts_with(b"+"));
    common::until("both shards to save", || (0..2).all(|i| dump(i).exists()));
    saved_at(dir)
}

/// A catalog nothing was ever declared in still has a record: a save
/// keeps it, a rewrite keeps it, and a start from either takes it back.
#[test]
fn an_empty_catalog_keeps_its_record_through_saves_rewrites_and_restarts() {
    let dir = TmpDir::new("catalog-empty-record");
    let node = Node::primary(2, &dir);
    let first = saved(&node, dir.path());
    assert!(first.0 != b"0" && first.1 == b"0", "{first:?}");
    assert!(call(&mut node.wire(), "BGREWRITEAOF").starts_with(b"+"));
    common::until("every shard's log to be rewritten with the catalog", || {
        every_log_holds(dir.path(), b"XINTERNAL.CATALOG")
    });
    node.stop();
    // from the rewritten logs alone
    for i in 0..2 {
        std::fs::remove_file(dir.path().join(format!("dump-{i}.rdb"))).unwrap();
    }
    let node = Node::primary(2, &dir);
    assert_eq!(saved(&node, dir.path()), first, "after the rewrite");
    node.stop();
    // from the snapshots that save left
    let node = Node::primary(2, &dir);
    assert_eq!(saved(&node, dir.path()), first, "after the save");
    node.stop();
}

/// A replica takes its catalog from its primary only: queries that would
/// earn a table an engine-declared path on a primary declare nothing on
/// it, and the primary's next change still applies.
#[test]
fn a_replica_declares_nothing_for_the_queries_it_refuses() {
    let (pdir, rdir) = (TmpDir::new("catalog-primary-auto"), TmpDir::new("catalog-replica-auto"));
    let primary = Node::primary(2, &pdir);
    let replica = Node::replica(&primary, 2, &rdir);
    let (mut p, mut r) = (primary.wire(), replica.wire());
    ok(&mut p, "TABLE.DECLARE auto PREFIX a: PK id COLUMN id str COLUMN age i64 AUTODECLARE 2");
    assert_eq!(call(&mut p, "HSET a:1 id 1 age 30"), b":2\r\n");
    let want = steady(&primary);
    assert_eq!(settled(&replica, &want), want);
    for _ in 0..kevy_index::AUTODECLARE_AFTER {
        assert!(call(&mut r, "IDX.QUERY auto.age RANGE 0 100").starts_with(b"-ERR"));
    }
    assert_eq!(catalog(&replica), want);
    ok(&mut p, "IDX.CREATE later ON PREFIX a: FIELD age TYPE i64 KIND range");
    let want = steady(&primary);
    assert!(want[0].contains("\"later\""), "{want:?}");
    assert_eq!(settled(&replica, &want), want);
    replica.stop();
    primary.stop();
}

/// A sidecar that does not parse is not taken for an empty catalog: it
/// stays where it is.
#[test]
fn a_sidecar_that_does_not_parse_stays() {
    let dir = TmpDir::new("catalog-bad-sidecar");
    let bad = dir.path().join("index-catalog.meta");
    std::fs::write(&bad, b"not a catalog").unwrap();
    let node = Node::primary(2, &dir);
    assert_eq!(names(&call(&mut node.wire(), "IDX.LIST")), Vec::<String>::new());
    node.stop();
    assert_eq!(std::fs::read(&bad).unwrap(), b"not a catalog");
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

/// Copy into `dir` the part of the 6.4.0 fixture `part` keeps: the
/// keyspace (logs and layout) or the catalog files.
fn lay_out_6_4(dir: &Path, part: impl Fn(&str) -> bool) {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/data-dir-6.4.0");
    for entry in std::fs::read_dir(&fixture).unwrap() {
        let from = entry.unwrap().path();
        let name = from.file_name().unwrap().to_str().unwrap().trim_end_matches(".in").to_string();
        if part(&name) {
            std::fs::copy(&from, dir.join(name)).unwrap();
        }
    }
}

fn is_sidecar(name: &str) -> bool {
    name.ends_with("-catalog.meta")
}

/// A replica stays connected while its primary restarts on a release that
/// reads the 6.4 catalog files: the catalog the primary imports on that
/// start reaches the replica on the stream, as any catalog change does.
/// Before the fix the import went to the primary's log only, and the
/// replica, resuming where it stopped, held no catalog until the primary's
/// next catalog command.
#[test]
fn a_connected_replica_gets_the_catalog_its_primary_imports_from_6_4_files() {
    let pdir = TmpDir::new("catalog-import-primary");
    let rdir = TmpDir::new("catalog-import-replica");
    // a 6.4 primary: its catalog lives in files it does not replicate
    lay_out_6_4(pdir.path(), |n| !is_sidecar(n));
    let primary = Node::primary(2, &pdir);
    let replica = Node::replica(&primary, 2, &rdir);
    let mut r = replica.wire();
    common::until("the replica to catch up", || call(&mut r, "EXISTS user:1") == b":1\r\n");
    assert_eq!(names(&call(&mut r, "IDX.LIST")), Vec::<String>::new());
    let port = primary.port;
    primary.stop();
    lay_out_6_4(pdir.path(), is_sidecar);
    let primary = Node::primary_restarted(port, 2, &pdir);
    let want = steady(&primary);
    assert_eq!(want[0], r#"IDX.LIST: ["user_age", "user_plan", "users.email"]"#);
    // the replica is following the restarted primary
    assert_eq!(call(&mut primary.wire(), "SET after 1"), b"+OK\r\n");
    common::until("the replica to reconnect", || call(&mut r, "GET after") == b"$1\r\n1\r\n");
    assert_eq!(settled(&replica, &want), want);
    replica.stop();
    primary.stop();
}
