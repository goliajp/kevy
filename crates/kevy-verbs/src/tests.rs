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

/// The write column agrees with the registry's, and the lookup a caller
/// asks on every command agrees with the table.
#[test]
fn the_write_column_matches_the_registry() {
    let mut compared = 0;
    for v in VERBS {
        let Some(row) = OP_TABLE.iter().find(|o| o.name == v.name) else {
            panic!("{}: in VERBS but not in the registry", v.name);
        };
        assert_eq!(v.write, row.write, "{}: write column disagrees", v.name);
        assert_eq!(crate::is_write(v.name.as_bytes()), Some(v.write), "{}", v.name);
        compared += 1;
    }
    assert!(compared > 90);
    for name in ["PING", "COPY", "EVAL", "get"] {
        assert_eq!(crate::is_write(name.as_bytes()), None, "{name}");
    }
}

/// A blocking pop that pops is recorded as the plain pop it performed;
/// one that pops nothing asks for no record.
#[test]
fn a_blocking_pop_records_the_pop_it_performed() {
    let mut s = Store::new();
    assert_eq!(run(&mut s, "BLPOP q 0"), (Some(Effect::Unchanged), Vec::new()));
    assert_eq!(run(&mut s, "BLPOP q r 0"), (Some(Effect::Unchanged), Vec::new()));
    run(&mut s, "RPUSH q a b");
    let pop = |v: &str| Some(Effect::Record(vec![v.as_bytes().to_vec(), b"q".to_vec()]));
    assert_eq!(run(&mut s, "BLPOP q 0").0, pop("LPOP"));
    assert_eq!(run(&mut s, "BRPOP q 0").0, pop("RPOP"));
    assert_eq!(run(&mut s, "LLEN q").1, b":0\r\n");
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

#[test]
fn recording_a_deadline_reaps_nothing() {
    use kevy_store::{SetCondition, Store};
    use std::time::Duration;
    let mut store = Store::new();
    store.set(b"k", b"v".to_vec(), Some(Duration::from_millis(1)), SetCondition::Always);
    // the TTL runs out between the write and its record
    std::thread::sleep(Duration::from_millis(5));
    let f = crate::aof::deadline_frame(&store, b"k").expect("the deadline is recorded");
    let at: u64 = std::str::from_utf8(&f[2]).unwrap().parse().unwrap();
    let now =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis();
    assert!(u128::from(at) <= now, "recorded as the past deadline it is");
    assert_eq!(store.dbsize(), 1, "the record removed the key");
    assert_eq!(store.expired_keys_total(), 0, "the record counted an expiry");
}

#[test]
fn hrandfield_on_a_key_of_another_type_answers_wrongtype() {
    let mut s = Store::new();
    run(&mut s, "SET k v");
    let wrongtype = b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n";
    for cmd in ["HRANDFIELD k", "HRANDFIELD k 2", "HRANDFIELD k 2 WITHVALUES"] {
        assert_eq!(run(&mut s, cmd).1, wrongtype, "{cmd}");
    }
}

#[test]
fn zadd_takes_each_condition_and_refuses_the_clashing_ones() {
    let mut s = Store::new();
    assert_eq!(run(&mut s, "ZADD z XX 1 m").1, b":0\r\n", "XX adds no new member");
    assert_eq!(run(&mut s, "ZADD z 5 m").1, b":1\r\n");
    assert_eq!(run(&mut s, "ZADD z LT CH 7 m").1, b":0\r\n", "LT keeps the lower score");
    assert_eq!(run(&mut s, "ZADD z LT CH 3 m").1, b":1\r\n");
    assert_eq!(run(&mut s, "ZSCORE z m").1, b"$1\r\n3\r\n");
    let clash = b"-ERR GT, LT, and/or NX options at the same time are not compatible\r\n";
    for cmd in ["ZADD z GT LT 1 m", "ZADD z NX GT 1 m", "ZADD z NX LT 1 m"] {
        assert_eq!(run(&mut s, cmd).1, clash, "{cmd}");
    }
    // a condition on a key of another type is refused by type, as plain ZADD is
    let wrongtype = b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n";
    run(&mut s, "SET str v");
    assert_eq!(run(&mut s, "ZADD str XX 1 m").1, wrongtype);
}

#[test]
fn a_scan_refusal_displays_its_wire_text_without_the_code() {
    use crate::args::ScanOptsError;
    let cases = [
        (ScanOptsError::InvalidCursor, "invalid cursor"),
        (ScanOptsError::NotInteger, "value is not an integer or out of range"),
        (ScanOptsError::Syntax, "syntax error"),
    ];
    for (e, text) in cases {
        assert_eq!(e.to_string(), text);
        assert_eq!(e.as_wire(), format!("ERR {text}"));
    }
}

#[test]
fn the_default_claim_took_and_dropped_nothing() {
    use crate::aof::{Claim, Consumer};
    let c = Claim::default();
    assert!(c.is_empty());
    assert_eq!(c, Claim::new(Vec::new(), Vec::new(), Consumer::Existing));
}

#[test]
fn a_float_argument_takes_every_infinity_spelling_and_refuses_nan() {
    use crate::args::arg_f64;
    for s in ["inf", "+inf", "INF", "Infinity", "+InFiNiTy", " inf "] {
        assert_eq!(arg_f64(s.as_bytes()), Some(f64::INFINITY), "{s:?}");
    }
    for s in ["-inf", "-INFINITY", "-Inf"] {
        assert_eq!(arg_f64(s.as_bytes()), Some(f64::NEG_INFINITY), "{s:?}");
    }
    for s in ["nan", "NaN", "+nan", "infin", "infinityx", "", "0x1", "1_0"] {
        assert_eq!(arg_f64(s.as_bytes()), None, "{s:?}");
    }
    assert_eq!(arg_f64(b"\xff"), None);
    assert_eq!(arg_f64(b" -2.5e1\t"), Some(-25.0));
}

#[test]
fn zadd_and_set_options_match_in_any_case() {
    let mut s = Store::new();
    assert_eq!(run(&mut s, "ZADD z nx Ch 1 a").1, b":1\r\n");
    assert_eq!(run(&mut s, "ZADD z xX cH 2 a").1, b":1\r\n");
    assert_eq!(run(&mut s, "ZADD z iNcR 3 a").1, b"$1\r\n5\r\n");
    assert!(run(&mut s, "ZADD z Nx xx 1 a").1.starts_with(b"-ERR"));
    assert_eq!(run(&mut s, "ZADD z inf b -Inf c").1, b":2\r\n");
    assert!(run(&mut s, "ZADD z 4 d 5").1.starts_with(b"-ERR wrong number"));
    assert_eq!(run(&mut s, "ZADD z x d").1, b"-ERR value is not a valid float\r\n");
    assert!(run(&mut s, "ZADD z INCR 1 a 2 b").1.starts_with(b"-ERR INCR"));
    assert_eq!(run(&mut s, "ZRANGE z 0 -1").1, b"*3\r\n$1\r\nc\r\n$1\r\na\r\n$1\r\nb\r\n");
    assert_eq!(run(&mut s, "SET k v nX pX 100000").1, b"+OK\r\n");
    assert_eq!(run(&mut s, "SET k v Nx").1, b"$-1\r\n");
    assert!(run(&mut s, "SET k v bogus").1.starts_with(b"-ERR syntax"));
}
