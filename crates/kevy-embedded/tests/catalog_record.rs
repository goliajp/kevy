//! The index, view and table catalog is recorded state, as on the
//! server: it comes back from the log, a snapshot, a rewritten log and a
//! reshard with no side file written, it reaches a replica in its full
//! sync and on the stream, and a replica declares nothing of its own.

#![cfg(all(feature = "index", feature = "replicate", not(target_arch = "wasm32")))]

use std::path::Path;
use std::time::{Duration, Instant};

use kevy_embedded::{Config, KevyError, Store};
use kevy_tmpdir::TmpDir;

const READONLY: &[u8] = b"-READONLY You can't write against a read only replica.\r\n";
const SIDECARS: [&str; 3] = ["index-catalog.meta", "view-catalog.meta", "table-catalog.meta"];

fn call(s: &Store, cmd: &str) -> Vec<u8> {
    let argv: Vec<Vec<u8>> = cmd.split(' ').map(|p| p.as_bytes().to_vec()).collect();
    let mut out = Vec::new();
    s.dispatch_argv(&argv, &mut out);
    out
}

fn ok(s: &Store, cmd: &str) {
    assert_eq!(String::from_utf8_lossy(&call(s, cmd)), "+OK\r\n", "{cmd}");
}

/// An index, a view over it, a table (which compiles its own index), and
/// an index created and dropped again.
fn declare(s: &Store) {
    for i in 0..20 {
        assert_eq!(call(s, &format!("HSET user:{i} age {i}")), b":1\r\n");
    }
    ok(s, "IDX.CREATE age ON PREFIX user: FIELD age TYPE i64 KIND range");
    ok(s, "IDX.CREATE gone ON PREFIX user: FIELD age TYPE i64 KIND range");
    assert_eq!(call(s, "IDX.DROP gone"), b":1\r\n");
    ok(s, "VIEW.CREATE adults QUERY age RANGE 18 200 ORDER BY age");
    ok(
        s,
        "TABLE.DECLARE users PREFIX users: PK id COLUMN id i64 COLUMN email str INDEX email unique",
    );
}

/// The names a `*.LIST` reply holds, in order (the counters beside them
/// move with every read).
fn names(reply: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(reply);
    let lines: Vec<&str> = text.split("\r\n").collect();
    lines
        .windows(3)
        .filter(|w| w[0] == "name" && w[1].starts_with('$'))
        .map(|w| w[2].to_string())
        .collect()
}

/// What a store's catalog holds and answers: the names of each kind and
/// a query through the index and the view.
fn catalog(s: &Store) -> Vec<String> {
    let mut seen = Vec::new();
    for list in ["IDX.LIST", "VIEW.LIST", "TABLE.LIST"] {
        seen.push(format!("{list}: {:?}", names(&call(s, list))));
    }
    for query in ["IDX.QUERY age RANGE 15 17", "VIEW.QUERY adults"] {
        seen.push(format!("{query}: {}", String::from_utf8_lossy(&call(s, query))));
    }
    seen
}

fn declared(s: &Store) -> Vec<String> {
    declare(s);
    let want = catalog(s);
    assert!(want[0].contains("age") && !want[0].contains("gone"), "{want:?}");
    assert!(want[1].contains("adults") && want[2].contains("users"), "{want:?}");
    assert!(want[3].contains("user:16") && want[4].contains("user:19"), "{want:?}");
    want
}

fn settled(s: &Store, want: &[String]) -> Vec<String> {
    let started = Instant::now();
    let mut got = catalog(s);
    while got != want && started.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(20));
        got = catalog(s);
    }
    got
}

fn no_sidecars(dir: &Path) -> bool {
    SIDECARS.iter().all(|name| !dir.join(name).exists())
}

fn config(dir: &TmpDir, shards: usize) -> Config {
    Config::default().with_persist(dir.path()).with_shards(shards).with_ttl_reaper_manual()
}

/// Declare on a two-shard store, run `then`, reopen with `reopen_shards`
/// shards, and expect the same catalog, twice.
fn survives(label: &str, reopen_shards: usize, then: impl FnOnce(&Store)) {
    let dir = TmpDir::new(label);
    let s = Store::open(config(&dir, 2)).unwrap();
    let want = declared(&s);
    then(&s);
    drop(s);
    assert!(no_sidecars(dir.path()), "{label}: a side file was written");
    for round in ["reopened", "reopened again"] {
        let s = Store::open(config(&dir, reopen_shards)).unwrap();
        assert_eq!(catalog(&s), want, "{label}: {round}");
    }
}

#[test]
fn the_catalog_comes_back_from_the_log() {
    survives("emb-catalog-log", 2, |_| {});
}

#[test]
fn the_catalog_comes_back_from_a_snapshot() {
    survives("emb-catalog-save", 2, |s| assert!(s.save_snapshot().unwrap()));
}

#[test]
fn the_catalog_comes_back_from_a_rewritten_log() {
    survives("emb-catalog-rewrite", 2, |s| assert!(s.rewrite_aof().unwrap().is_some()));
}

#[test]
fn the_catalog_comes_back_from_a_reshard() {
    survives("emb-catalog-reshard", 4, |_| {});
}

fn writer() -> (Store, String) {
    let s = Store::open(Config::default().with_embed_writer("127.0.0.1:0")).unwrap();
    let addr = s.writer_addr().unwrap().to_string();
    (s, addr)
}

#[test]
fn a_replica_gets_the_catalog_in_its_full_sync_and_on_the_stream() {
    let (primary, addr) = writer();
    let want = declared(&primary);
    let replica = Store::open_replica(&addr).unwrap();
    assert_eq!(settled(&replica, &want), want, "full sync");
    ok(&primary, "IDX.CREATE later ON PREFIX user: FIELD age TYPE i64 KIND range");
    assert_eq!(call(&primary, "VIEW.DROP adults"), b":1\r\n");
    let want = catalog(&primary);
    assert!(want[0].contains("later") && !want[1].contains("adults"), "{want:?}");
    assert_eq!(settled(&replica, &want), want, "stream");
}

#[test]
fn a_replica_declares_nothing_of_its_own() {
    let (primary, addr) = writer();
    let replica = Store::open_replica(&addr).unwrap();
    let mine = "IDX.CREATE mine ON PREFIX user: FIELD age TYPE i64 KIND range";
    assert_eq!(call(&replica, mine), READONLY);
    for cmd in ["IDX.DROP age", "VIEW.DROP adults", "TABLE.DROP users"] {
        assert_eq!(call(&replica, cmd), READONLY, "{cmd}");
    }
    let typed = replica.idx_create(
        b"mine",
        b"user:",
        b"age",
        kevy_embedded::IndexValType::I64,
        kevy_embedded::IndexKind::Range,
    );
    assert!(matches!(typed, Err(KevyError::ReadOnly)), "{typed:?}");
    assert!(matches!(replica.idx_drop(b"age"), Err(KevyError::ReadOnly)));
    assert!(call(&replica, "IDX.LIST").starts_with(b"*0"), "the replica declared its own");
    drop(primary);
}

/// A side file that does not parse is not taken for an empty catalog:
/// it stays where it is.
#[test]
fn a_side_file_that_does_not_parse_stays() {
    let dir = TmpDir::new("emb-catalog-bad-sidecar");
    std::fs::write(dir.path().join(SIDECARS[0]), b"not a catalog").unwrap();
    let s = Store::open(config(&dir, 2)).unwrap();
    assert!(names(&call(&s, "IDX.LIST")).is_empty());
    drop(s);
    assert_eq!(std::fs::read(dir.path().join(SIDECARS[0])).unwrap(), b"not a catalog");
}

/// A side file that cannot be read at all is not taken for an empty
/// catalog either.
#[test]
fn a_side_file_that_cannot_be_read_stays() {
    let dir = TmpDir::new("emb-catalog-unreadable-sidecar");
    std::fs::create_dir(dir.path().join(SIDECARS[1])).unwrap();
    let s = Store::open(config(&dir, 2)).unwrap();
    assert!(names(&call(&s, "VIEW.LIST")).is_empty());
    drop(s);
    assert!(dir.path().join(SIDECARS[1]).is_dir());
}

/// A copy of a directory 6.4.0 wrote, which keeps its catalog in side
/// files.
fn from_6_4(label: &str) -> TmpDir {
    let dir = TmpDir::new(label);
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/data-dir-6.4.0");
    for entry in std::fs::read_dir(&fixture).unwrap() {
        let from = entry.unwrap().path();
        let name = from.file_name().unwrap().to_str().unwrap().trim_end_matches(".in").to_string();
        std::fs::copy(&from, dir.path().join(name)).unwrap();
    }
    dir
}

fn every_sidecar(dir: &Path) -> bool {
    SIDECARS.iter().all(|name| dir.join(name).exists())
}

/// Without a log nothing durable holds the imported catalog, so the side
/// files it came from stay.
#[test]
fn a_store_without_a_log_imports_the_side_files_and_keeps_them() {
    let dir = from_6_4("emb-catalog-from-6.4-no-log");
    let s = Store::open(config(&dir, 2).without_aof()).unwrap();
    let got = catalog(&s);
    assert!(got[0].contains("user_age") && got[1].contains("adults"), "{got:?}");
    drop(s);
    assert!(every_sidecar(dir.path()), "a side file went");
}

/// A replica takes its catalog from its primary, never from side files.
#[test]
fn a_replica_leaves_the_side_files_to_its_primary() {
    let dir = from_6_4("emb-catalog-from-6.4-replica");
    // an upstream nobody listens on: the store stays a replica
    let s = Store::open(config(&dir, 2).with_replica_upstream("127.0.0.1:1")).unwrap();
    assert!(names(&call(&s, "IDX.LIST")).is_empty());
    drop(s);
    assert!(every_sidecar(dir.path()), "a side file went");
}

/// A directory 6.4.0 wrote keeps its catalog in side files; the first
/// open reads them once, records the catalog, and removes them.
#[test]
fn a_6_4_directory_brings_its_catalog_into_the_log() {
    let dir = from_6_4("emb-catalog-from-6.4");
    let s = Store::open(config(&dir, 2)).unwrap();
    let got = catalog(&s);
    for name in ["user_age", "user_plan", "users.email"] {
        assert!(got[0].contains(name), "{got:?}");
    }
    assert!(got[1].contains("adults") && got[2].contains("users"), "{got:?}");
    assert!(got[4].contains("user:1"), "{got:?}");
    assert!(no_sidecars(dir.path()), "the side files stayed");
    drop(s);
    let s = Store::open(config(&dir, 2)).unwrap();
    assert_eq!(catalog(&s), got);
}

/// The catalog frame shard 0's snapshot in `dir` carries.
fn saved_frame(dir: &Path) -> Option<kevy_persist::Argv> {
    let file = std::fs::File::open(dir.join("dump-0.rdb")).unwrap();
    let mut keys = kevy_rt::Store::new();
    kevy_persist::load_snapshot_with_aux(&mut keys, std::io::BufReader::new(file), |_| true)
        .unwrap()
}

/// A directory from before 7.0 opens and its catalog gets a lineage: from
/// the side files it imports (recorded at version 1), or, with none, one
/// of its own at version 0 that the next open keeps.
#[test]
fn a_directory_from_before_7_0_opens_with_a_lineage() {
    for (label, sidecars, version) in [
        ("emb-catalog-lineage-sidecars", true, &b"1"[..]),
        ("emb-catalog-lineage-none", false, b"0"),
    ] {
        let dir = from_6_4(label);
        if !sidecars {
            for name in SIDECARS {
                std::fs::remove_file(dir.path().join(name)).unwrap();
            }
        }
        let s = Store::open(config(&dir, 2)).unwrap();
        assert!(s.save_snapshot().unwrap());
        let first = saved_frame(dir.path())
            .unwrap_or_else(|| panic!("{label}: the snapshot carries no catalog record"));
        assert!(first[1] != b"0"[..] && first[2] == *version, "{label}: {first:?}");
        drop(s);
        let s = Store::open(config(&dir, 2)).unwrap();
        assert!(s.save_snapshot().unwrap());
        let again = saved_frame(dir.path()).unwrap();
        assert_eq!((&again[1], &again[2]), (&first[1], &first[2]), "{label}");
    }
}
