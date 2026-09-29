use kevy_resp::Argv;
use kevy_resp::ops_table::OP_TABLE;
use kevy_store::Store;

use crate::{Effect, VERBS, exec, verb};

fn run(store: &mut Store, cmd: &str) -> (Option<Effect>, Vec<u8>) {
    let argv = Argv::from(cmd.split(' ').map(|s| s.as_bytes().to_vec()).collect::<Vec<_>>());
    let mut buf = [0u8; 32];
    let up = crate::args::upper_verb(&argv[0], &mut buf).to_vec();
    let mut out = Vec::new();
    (exec(store, &up, &argv, &mut out), out)
}

#[test]
fn the_table_is_sorted_and_unique() {
    assert!(VERBS.len() > 90, "the table lost rows: {}", VERBS.len());
    for w in VERBS.windows(2) {
        assert!(w[0].name.as_bytes() < w[1].name.as_bytes(), "{} before {}", w[0].name, w[1].name);
    }
    for v in VERBS {
        assert_eq!(verb(v.name.as_bytes()), Some(v));
    }
}

/// The feature's rows are exactly the verbs `is_streams_geo` names, so a
/// caller filtering by it keeps out the whole family and nothing else.
#[test]
fn the_feature_rows_are_the_streams_geo_names() {
    let named = VERBS.iter().filter(|v| crate::is_streams_geo(v.name.as_bytes())).count();
    assert_eq!(named, if cfg!(feature = "streams-geo") { 23 } else { 0 });
    for name in ["GET", "GETEX", "EXPIRE", "PEXPIREAT", "HEXPIRE"] {
        assert!(!crate::is_streams_geo(name.as_bytes()), "{name}");
    }
    // the read-only radius twins have no registry row, so no table row
    for name in ["GEORADIUS_RO", "GEORADIUSBYMEMBER_RO"] {
        assert_eq!(run(&mut Store::new(), name).0, None, "{name}");
    }
}

/// The table and `exec` answer for the same verbs: every row runs, and
/// nothing outside the table does. The candidates are every registry
/// row plus the table itself, so a verb added to one and not the other
/// fails here.
#[test]
fn exec_answers_exactly_the_table() {
    let mut store = Store::new();
    let mut probed = 0;
    for name in OP_TABLE.iter().map(|o| o.name).chain(VERBS.iter().map(|v| v.name)) {
        let (effect, out) = run(&mut store, name);
        let listed = verb(name.as_bytes()).is_some();
        assert_eq!(effect.is_some(), listed, "{name}: exec and VERBS disagree");
        assert_eq!(!out.is_empty(), listed || effect.is_some(), "{name}: answered nothing");
        probed += 1;
    }
    assert!(probed > 250, "probed only {probed}");
}

/// The write column agrees with the registry's, except where the
/// registry records how the server logs a verb rather than its effect.
#[test]
fn the_write_column_matches_the_registry() {
    // RENAME / RENAMENX are routed at the op level and BLPOP / BRPOP are
    // logged as the pop they perform, so the registry calls all four
    // reads; each of them does change the keyspace
    const ROUTED: &[&str] = &["BLPOP", "BRPOP", "RENAME", "RENAMENX"];
    let mut compared = 0;
    for v in VERBS {
        let Some(row) = OP_TABLE.iter().find(|o| o.name == v.name) else {
            panic!("{}: in VERBS but not in the registry", v.name);
        };
        if ROUTED.contains(&v.name) {
            assert!(v.write && !row.write, "{}: the routing exception no longer holds", v.name);
            continue;
        }
        assert_eq!(v.write, row.write, "{}: write column disagrees", v.name);
        compared += 1;
    }
    assert!(compared > 90);
}

#[test]
fn a_write_that_changes_nothing_says_so() {
    let mut s = Store::new();
    assert_eq!(run(&mut s, "SET k v NX").0, Some(Effect::Write));
    assert_eq!(run(&mut s, "SET k w NX"), (Some(Effect::Unchanged), b"$-1\r\n".to_vec()));
    assert_eq!(run(&mut s, "SET gone v XX EX 5").0, Some(Effect::Unchanged));
    assert_eq!(run(&mut s, "HDEL h f").0, Some(Effect::Unchanged));
    assert_eq!(run(&mut s, "HSET h f v").0, Some(Effect::Write));
    assert_eq!(run(&mut s, "HDEL h f").0, Some(Effect::Write));
    assert_eq!(run(&mut s, "EXPIRE none 10").0, Some(Effect::Unchanged));
    assert_eq!(run(&mut s, "EXPIRE k 10").0, Some(Effect::Write));
    assert_eq!(run(&mut s, "PERSIST k").0, Some(Effect::Write));
    assert_eq!(run(&mut s, "PERSIST k").0, Some(Effect::Unchanged));
    assert_eq!(run(&mut s, "LPOP l").0, Some(Effect::Unchanged));
    assert_eq!(run(&mut s, "GET k").0, Some(Effect::Read));
}

#[test]
fn spop_is_recorded_as_what_it_removed() {
    let mut s = Store::new();
    assert_eq!(run(&mut s, "SPOP s").0, Some(Effect::Skip));
    run(&mut s, "SADD s a");
    let (effect, out) = run(&mut s, "SPOP s 5");
    assert_eq!(out, b"*1\r\n$1\r\na\r\n");
    assert_eq!(effect, Some(Effect::Record(vec![b"SREM".to_vec(), b"s".to_vec(), b"a".to_vec()])));
    assert_eq!(run(&mut s, "SRANDMEMBER s").0, Some(Effect::Read));
}

/// A store serving from a cached clock (as the server's shards do) whose
/// cache is older than a key's deadline: the key is past its deadline by
/// the fresh clock but not yet by the cached one.
fn stale_clock_store_with_lapsed_key() -> Store {
    let mut s = Store::new();
    s.set_cached_clock(true);
    s.refresh_clock();
    run(&mut s, "SET k v PX 20");
    std::thread::sleep(std::time::Duration::from_millis(60));
    s
}

/// EXPIRE decides whether the key exists with the same probe that writes
/// it, so a key lapsed between two clocks gets one answer, not two: a
/// non-positive TTL and a positive one both find it gone.
#[test]
fn expire_decides_existence_with_the_probe_that_writes() {
    let lapsed =
        ["EXPIRE k 0", "PEXPIRE k -1", "EXPIRE k 100", "PEXPIREAT k 1", "EXPIREAT k 99999999999"];
    for cmd in lapsed {
        let mut s = stale_clock_store_with_lapsed_key();
        assert_eq!(run(&mut s, cmd), (Some(Effect::Unchanged), b":0\r\n".to_vec()), "{cmd}");
        assert_eq!(s.dbsize(), 0, "{cmd}");
    }
    let mut s = Store::new();
    run(&mut s, "SET k v");
    assert_eq!(run(&mut s, "EXPIRE k 0"), (Some(Effect::Write), b":1\r\n".to_vec()));
    assert_eq!(run(&mut s, "EXPIRE k 0"), (Some(Effect::Unchanged), b":0\r\n".to_vec()));
    run(&mut s, "SET k v");
    assert_eq!(run(&mut s, "PEXPIREAT k 1"), (Some(Effect::Write), b":1\r\n".to_vec()));
    assert_eq!(s.dbsize(), 0);
}
