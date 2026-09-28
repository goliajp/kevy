//! A store with nothing to record to (no AOF, no replica source, no feed)
//! still runs group reads and claims in full: the outcome frames are not
//! built, but the commands' effects are the same.

use kevy_embedded::{Config, Store};

fn run(s: &Store, parts: &[&[u8]]) -> Vec<u8> {
    let argv: Vec<Vec<u8>> = parts.iter().map(|p| p.to_vec()).collect();
    let mut out = Vec::new();
    s.dispatch_argv(&argv, &mut out);
    out
}

#[test]
fn group_reads_and_claims_run_without_a_log() {
    let s = Store::open(Config::default()).expect("open");
    assert_eq!(run(&s, &[b"XADD", b"s", b"1-1", b"f", b"v"]), b"$3\r\n1-1\r\n");
    assert_eq!(run(&s, &[b"XGROUP", b"CREATE", b"s", b"g", b"0"]), b"+OK\r\n");
    let read = run(&s, &[b"XREADGROUP", b"GROUP", b"g", b"c1", b"STREAMS", b"s", b">"]);
    assert!(String::from_utf8_lossy(&read).contains("1-1"), "{read:?}");
    let pending = |s: &Store| run(s, &[b"XPENDING", b"s", b"g", b"-", b"+", b"10"]);
    assert!(String::from_utf8_lossy(&pending(&s)).contains("c1"));
    let claim = run(&s, &[b"XCLAIM", b"s", b"g", b"c2", b"0", b"1-1", b"JUSTID"]);
    assert_eq!(claim, b"*1\r\n$3\r\n1-1\r\n");
    let after = String::from_utf8_lossy(&pending(&s)).into_owned();
    assert!(after.contains("c2") && !after.contains("c1"), "{after}");
    // a group read against a key of another type refuses, and records nothing
    assert_eq!(run(&s, &[b"SET", b"str", b"v"]), b"+OK\r\n");
    let wrong = run(&s, &[b"XREADGROUP", b"GROUP", b"g", b"c1", b"STREAMS", b"str", b">"]);
    assert!(wrong.starts_with(b"-"), "{:?}", String::from_utf8_lossy(&wrong));
}
