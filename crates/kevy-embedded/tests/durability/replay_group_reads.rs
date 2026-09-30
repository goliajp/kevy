//! A consumer group's read counter and each consumer's last active time
//! come back from the log, from the log a rewrite compacts it to, and from
//! a snapshot as they were: `XINFO STREAM … FULL`, which shows both and
//! every time as an absolute one, answers the same bytes after each.

#![cfg(all(feature = "persist", feature = "streams-geo"))]

use kevy_embedded::{Config, Store};

fn open(dir: &std::path::Path) -> Store {
    Store::open(Config::default().with_persist(dir).with_shards(1).with_ttl_reaper_manual())
        .expect("open")
}

fn call(s: &Store, cmd: &str) -> String {
    let argv: Vec<Vec<u8>> = cmd.split(' ').map(|p| p.as_bytes().to_vec()).collect();
    let mut out = Vec::new();
    s.dispatch_argv(&argv, &mut out);
    String::from_utf8(out).unwrap()
}

/// Groups holding every kind of counter and consumer (see the server's
/// twin of this test): read into, set by `ENTRIESREAD`, moved by `SETID`,
/// left unknown by a deletion; a reader, a `NOACK` reader, a consumer only
/// made, a claimer, a claim that took nothing.
fn build(s: &Store) -> String {
    for cmd in [
        "XADD s 1-1 f v",
        "XADD s 2-1 f v",
        "XADD s 3-1 f v",
        "XADD s 4-1 f v",
        "XADD s 5-1 f v",
        "XGROUP CREATE s g 0",
        "XGROUP CREATE s h $ ENTRIESREAD 2",
        "XGROUP CREATE s k 0",
        "XREADGROUP GROUP g reader COUNT 2 STREAMS s >",
    ] {
        assert!(!call(s, cmd).starts_with('-'), "{cmd}");
    }
    std::thread::sleep(std::time::Duration::from_millis(30));
    for cmd in
        ["XREADGROUP GROUP g quiet COUNT 1 NOACK STREAMS s >", "XGROUP CREATECONSUMER s g made"]
    {
        assert!(!call(s, cmd).starts_with('-'), "{cmd}");
    }
    std::thread::sleep(std::time::Duration::from_millis(30));
    for cmd in [
        "XCLAIM s g claimer 0 1-1 JUSTID",
        "XAUTOCLAIM s g empty 99999999 0",
        "XREADGROUP GROUP k z COUNT 1 STREAMS s >",
        "XGROUP SETID s k 1-1 ENTRIESREAD 7",
        "XDEL s 5-1",
    ] {
        assert!(!call(s, cmd).starts_with('-'), "{cmd}");
    }
    let full = call(s, "XINFO STREAM s FULL");
    assert!(full.contains("$12\r\nentries-read\r\n:7\r\n"), "{full}");
    assert_eq!(full.matches("$11\r\nactive-time\r\n:-1\r\n").count(), 3, "{full}");
    assert_eq!(full.matches("$11\r\nactive-time\r\n:17").count(), 3, "{full}");
    full
}

#[test]
fn read_counters_and_active_times_survive_the_log_and_its_rewrite() {
    let dir = kevy_tmpdir::TmpDir::new("replay-group-reads");
    let want = build(&open(dir.path()));
    {
        let s = open(dir.path());
        assert_eq!(call(&s, "XINFO STREAM s FULL"), want, "from the log");
        s.rewrite_aof().expect("rewrite");
    }
    assert_eq!(call(&open(dir.path()), "XINFO STREAM s FULL"), want, "from the rewritten log");
}

#[test]
fn read_counters_and_active_times_survive_a_snapshot() {
    let dir = kevy_tmpdir::TmpDir::new("snapshot-group-reads");
    let want = {
        let s = open(dir.path());
        let want = build(&s);
        assert!(s.save_snapshot().expect("save"));
        want
    };
    assert!(dir.path().join("dump-0.rdb").exists(), "no snapshot was written");
    assert_eq!(call(&open(dir.path()), "XINFO STREAM s FULL"), want, "from the snapshot");
}
