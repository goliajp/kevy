//! The engine has two write paths: the typed methods, and the argv path
//! every language binding uses, which runs the command layer the server
//! shares. They log differently shaped records for the same change, so
//! this holds them to the thing that matters: the same writes, through
//! either path, reopen to the same keyspace with the same deadlines.

#![cfg(feature = "persist")]

use std::time::Duration;

use kevy_embedded::{Config, Store};

const SECS: Duration = Duration::from_secs(100);

fn cfg(dir: &std::path::Path) -> Config {
    Config::default().with_persist(dir).with_shards(4).with_ttl_reaper_manual()
}

fn typed(s: &Store) {
    s.set_with_ttl(b"t", b"v", SECS).unwrap();
    s.setnx(b"n", b"1").unwrap();
    s.setnx(b"n", b"2").unwrap();
    s.incr_by(b"c", 5).unwrap();
    s.hset(b"h", &[(b"f", b"v"), (b"g", b"w")]).unwrap();
    s.hdel(b"h", &[b"g", b"missing"]).unwrap();
    s.rpush(b"l", &[b"a", b"b", b"c"]).unwrap();
    s.lpop(b"l", 1).unwrap();
    s.sadd(b"s", &[b"x"]).unwrap();
    s.spop(b"s", 1).unwrap();
    s.sadd(b"s2", &[b"y"]).unwrap();
    s.zadd(b"z", &[(1.0, b"a"), (2.5, b"b")]).unwrap();
    s.zrem(b"z", &[b"a"]).unwrap();
    s.set(b"e", b"v").unwrap();
    s.expire(b"e", SECS).unwrap();
    s.set(b"x", b"v").unwrap();
    s.getex(b"x", SECS).unwrap();
    s.set(b"gone", b"v").unwrap();
    s.del(&[b"gone"]).unwrap();
}

const ARGV: &[&str] = &[
    "SET t v EX 100",
    "SETNX n 1",
    "SETNX n 2",
    "INCRBY c 5",
    "HSET h f v g w",
    "HDEL h g missing",
    "RPUSH l a b c",
    "LPOP l",
    "SADD s x",
    "SPOP s",
    "SADD s2 y",
    "ZADD z 1 a 2.5 b",
    "ZREM z a",
    "SET e v",
    "EXPIRE e 100",
    "SET x v",
    "GETEX x EX 100",
    "SET gone v",
    "DEL gone",
];

fn by_argv(s: &Store) {
    for cmd in ARGV {
        let argv: Vec<Vec<u8>> = cmd.split(' ').map(|p| p.as_bytes().to_vec()).collect();
        let mut out = Vec::new();
        s.dispatch_argv(&argv, &mut out);
        assert!(!out.starts_with(b"-"), "{cmd}: {}", String::from_utf8_lossy(&out));
    }
}

/// Write through one path, close, wait long enough that a TTL counted
/// again from the replay would show, and reopen.
fn reopened(tag: &str, write: impl FnOnce(&Store)) -> (Store, kevy_tmpdir::TmpDir) {
    let dir = kevy_tmpdir::TmpDir::new(tag);
    let s = Store::open(cfg(dir.path())).expect("open");
    write(&s);
    drop(s);
    std::thread::sleep(Duration::from_millis(1200));
    (Store::open(cfg(dir.path())).expect("reopen"), dir)
}

#[test]
fn both_write_paths_reopen_to_the_same_keyspace() {
    let (a, _da) = reopened("agree-typed", typed);
    let (b, _db) = reopened("agree-argv", by_argv);
    let (count, digest) = a.prefix_digest(b"");
    // t n c h l s2 z e x: `s` lost its one member to SPOP, `gone` was deleted
    assert_eq!(count, 9, "the typed path left {count} keys, not the nine the setup makes");
    assert_eq!((count, digest), b.prefix_digest(b""), "the two paths reopened differently");
    for key in [&b"t"[..], b"e", b"x"] {
        let (ta, tb) = (a.ttl_ms(key), b.ttl_ms(key));
        let key = String::from_utf8_lossy(key);
        assert!(ta > 0 && tb > 0, "{key}: a deadline was lost ({ta} / {tb})");
        assert!(ta <= 99_000 && tb <= 99_000, "{key}: a deadline was re-anchored ({ta} / {tb})");
    }
    for key in [&b"n"[..], b"c", b"h", b"l", b"s2", b"z"] {
        assert_eq!(a.ttl_ms(key), -1);
        assert_eq!(b.ttl_ms(key), -1);
    }
}
