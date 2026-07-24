//! The tiering transparency suite (capacity arc, RFC
//! 2026-07-24-v5-capacity-arc §2 B9/B12) — dispatch-oracle genre, but the
//! two sides are TWO EMBEDDED STORES fed the same op sequence: one untiered,
//! one tiered with deterministic demotion points (the `KEVY_TEST_FORCE_DEMOTE`
//! seam, T3). Semantic replies must be byte-identical; memory-reporting
//! replies (INFO/MEMORY USAGE/DEBUG) are shape-compared — they legitimately
//! differ under tiering.
//!
//! T0 state: the harness + op sequence + compare machinery exist and
//! self-check green (untiered vs untiered). The tiered side is `#[ignore]`d
//! with a PENDING(T3) message — bench/tiergate.sh carries the red until the
//! `Value::Cold` funnel lands and this suite's ignore is lifted. The named
//! B9 specials (NX/XX on cold, WRONGTYPE without a pread, DEL/RENAME/
//! FLUSHALL, EXPIRE family, field-TTL survival across a demote/promote round
//! trip, WATCH not bumped by demotion, SCAN/KEYS/DBSIZE seeing cold keys)
//! are already present in the sequence below, marked with `// B9:` — the
//! tiered run will interleave FORCE_DEMOTE at every marker.

use kevy_embedded::{Config, Store};

fn reply(s: &Store, argv: &[&[u8]]) -> Vec<u8> {
    let owned: Vec<Vec<u8>> = argv.iter().map(|a| a.to_vec()).collect();
    let mut out = Vec::new();
    s.dispatch_argv(&owned, &mut out);
    out
}

/// How one command's two replies are held together.
enum Cmp {
    /// Byte-for-byte identical (the default — transparency's meaning).
    Exact,
    /// Same first byte (RESP kind) + same element count for arrays —
    /// for randomized/order-free or memory-reporting replies.
    Shape,
}
use Cmp::{Exact, Shape};

fn first_count(reply: &[u8]) -> i64 {
    let nl = reply.windows(2).position(|w| w == b"\r\n").expect("resp header");
    std::str::from_utf8(&reply[1..nl]).unwrap_or("0").parse().unwrap_or(0)
}

fn assert_match(what: &str, cmp: &Cmp, a: &[u8], b: &[u8]) {
    match cmp {
        Exact => assert_eq!(
            a,
            b,
            "transparency broken for `{what}`: {:?} vs {:?}",
            String::from_utf8_lossy(a),
            String::from_utf8_lossy(b)
        ),
        Shape => {
            assert_eq!(a.first(), b.first(), "reply kind differs for `{what}`");
            if a.first() == Some(&b'*') {
                assert_eq!(first_count(a), first_count(b), "element count differs for `{what}`");
            }
        }
    }
}

/// The op sequence. `// B9:` markers are the deterministic demotion points
/// the tiered run (T3+) will apply FORCE_DEMOTE at, so every special runs
/// against a genuinely cold key — never against eviction timing.
fn cases() -> Vec<(Vec<Vec<u8>>, Cmp)> {
    fn c(argv: &[&[u8]], cmp: Cmp) -> (Vec<Vec<u8>>, Cmp) {
        (argv.iter().map(|a| a.to_vec()).collect(), cmp)
    }
    vec![
        // strings, small + bulk (bulk = the spillable class)
        c(&[b"SET", b"s:small", b"v1"], Exact),
        c(&[b"SET", b"s:bulk", &[b'a'; 4096]], Exact),
        c(&[b"GET", b"s:bulk"], Exact), // B9: cold GET serves identical bytes
        c(&[b"STRLEN", b"s:bulk"], Exact),
        c(&[b"APPEND", b"s:bulk", b"tail"], Exact), // B9: RMW on cold pages in
        // NX/XX on an existing (cold) key
        c(&[b"SET", b"s:bulk", b"nope", b"NX"], Exact), // B9: NX must fail on cold
        c(&[b"SET", b"s:bulk", b"xxv", b"XX"], Exact),  // B9: XX must succeed on cold
        // WRONGTYPE against a cold string — must refuse without resurrecting
        c(&[b"LPUSH", b"s:bulk", b"x"], Exact), // B9: WRONGTYPE, zero pread
        c(&[b"HSET", b"s:bulk", b"f", b"v"], Exact), // B9: WRONGTYPE
        // hashes (rows) — the RDS payload class
        c(&[b"HSET", b"h:row", b"name", b"ada", b"dept", b"eng", b"age", b"36"], Exact),
        c(&[b"HGET", b"h:row", b"name"], Exact), // B9: cold-row field read
        c(&[b"HSET", b"h:row", b"age", b"37"], Exact), // B9: cold-row write pages in, no shadow
        c(&[b"HGETALL", b"h:row"], Shape), // field order is map order
        c(&[b"HDEL", b"h:row", b"dept"], Exact),
        c(&[b"HLEN", b"h:row"], Exact),
        // field TTL survival across demote/promote (B9 special)
        c(&[b"HSET", b"h:ttl", b"f1", b"v1", b"f2", b"v2"], Exact),
        c(&[b"HPEXPIRE", b"h:ttl", b"60000", b"FIELDS", b"1", b"f1"], Exact),
        c(&[b"HPERSIST", b"h:ttl", b"FIELDS", b"1", b"f1"], Exact), // B9: after a demote round trip
        // TTL family on cold keys
        c(&[b"SET", b"t:k", &[b'b'; 1024]], Exact),
        c(&[b"PEXPIRE", b"t:k", b"600000"], Exact), // B9: EXPIRE on cold
        c(&[b"PERSIST", b"t:k"], Exact),
        c(&[b"TYPE", b"t:k"], Exact), // B9: TYPE from type_tag, zero pread
        // keyspace ops over cold keys
        c(&[b"EXISTS", b"s:bulk", b"h:row", b"absent"], Exact),
        c(&[b"RENAME", b"t:k", b"t:k2"], Exact), // B9: RENAME moves stub, no read
        c(&[b"COPY", b"t:k2", b"t:k3"], Exact),
        c(&[b"DEL", b"t:k3"], Exact), // B9: DEL counts the cold key
        c(&[b"DBSIZE"], Exact),       // B9: cold keys counted
        c(&[b"KEYS", b"*"], Shape),   // B9: cold keys visible (order = map order)
        c(&[b"SCAN", b"0"], Shape),   // B9: cold keys swept
        c(&[b"RANDOMKEY"], Shape),
        // memory-reporting: legitimately differs under tiering
        c(&[b"MEMORY", b"USAGE", b"s:bulk"], Shape),
        // collections stay hot in v1 — still must behave identically
        c(&[b"RPUSH", b"l:q", b"a", b"b"], Exact),
        c(&[b"LRANGE", b"l:q", b"0", b"-1"], Exact),
        c(&[b"ZADD", b"z:s", b"1", b"m1"], Exact),
        c(&[b"ZSCORE", b"z:s", b"m1"], Exact),
        // wipe-out
        c(&[b"FLUSHALL"], Exact), // B9: clears cold tier too
        c(&[b"DBSIZE"], Exact),
    ]
}

fn run_pair(a: &Store, b: &Store) -> usize {
    let mut checked = 0;
    for (argv, cmp) in cases() {
        let refs: Vec<&[u8]> = argv.iter().map(|v| v.as_slice()).collect();
        let ra = reply(a, &refs);
        let rb = reply(b, &refs);
        let what = argv.iter().map(|x| String::from_utf8_lossy(x)).collect::<Vec<_>>().join(" ");
        assert_match(&what, &cmp, &ra, &rb);
        checked += 1;
    }
    checked
}

/// Harness self-check: two identical untiered stores must agree on every
/// case — proves the sequence + compare machinery before tiering exists.
#[test]
fn transparency_harness_self_check_untiered() {
    let a = Store::open(Config::default().with_ttl_reaper_manual()).expect("open a");
    let b = Store::open(Config::default().with_ttl_reaper_manual()).expect("open b");
    let checked = run_pair(&a, &b);
    assert!(checked >= 30, "sequence unexpectedly short: {checked}");
}

/// The real suite (B9): tiered vs untiered with FORCE_DEMOTE at every
/// `// B9:` marker. PENDING until T3 lands the `Value::Cold` funnel and the
/// tiering config knob — bench/tiergate.sh carries the red meanwhile.
#[test]
#[ignore = "PENDING(T3): needs the Value::Cold funnel + [tiering] config + KEVY_TEST_FORCE_DEMOTE seam"]
fn transparency_tiered_vs_untiered() {
    unimplemented!("T3: open one store tiered (tiny budget), interleave FORCE_DEMOTE at B9 markers, run_pair");
}
