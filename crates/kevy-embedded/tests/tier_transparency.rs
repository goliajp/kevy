//! The tiering transparency suite (capacity arc, RFC
//! 2026-07-24-v5-capacity-arc §2 B9/B12) — dispatch-oracle genre, but the
//! two sides are TWO EMBEDDED STORES fed the same op sequence: one untiered,
//! one tiered with deterministic demotion points (the store's
//! `debug_force_demote` seam — the `KEVY_TEST_FORCE_DEMOTE` genre, T3).
//! Semantic replies must be byte-identical; memory-reporting replies
//! (INFO/MEMORY USAGE/DEBUG) are shape-compared — they legitimately differ
//! under tiering.
//!
//! Each case carries its `// B9:` demotion point as data: the keys the
//! tiered run force-demotes BEFORE executing the op — so every special
//! runs against a genuinely cold key, never against eviction timing.
//! The named B9 specials (NX/XX on cold, WRONGTYPE without a pread,
//! DEL/RENAME/COPY, EXPIRE family, field-TTL survival across a
//! demote/promote round trip, cold RMW paging in, SCAN/KEYS/DBSIZE
//! seeing cold keys, FLUSHALL clearing the cold tier) are all in the
//! sequence. B12 (zero events + tier counters move, evictions do not)
//! is the second test.

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

/// One scripted op: argv + compare mode + the keys the tiered run
/// force-demotes right before dispatching it (the `// B9:` markers).
struct Case {
    argv: Vec<Vec<u8>>,
    cmp: Cmp,
    demote: &'static [&'static [u8]],
}

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

/// The op sequence. Every `demote` list is a deterministic demotion
/// point: those keys are warm + spillable at that moment, so the force
/// MUST succeed (asserted) — the op then runs against a cold key.
fn cases() -> Vec<Case> {
    fn c(argv: &[&[u8]], cmp: Cmp, demote: &'static [&'static [u8]]) -> Case {
        Case { argv: argv.iter().map(|a| a.to_vec()).collect(), cmp, demote }
    }
    const NONE: &[&[u8]] = &[];
    vec![
        // strings, small + bulk (bulk = the spillable class)
        c(&[b"SET", b"s:small", b"v1"], Exact, NONE),
        c(&[b"SET", b"s:bulk", &[b'a'; 4096]], Exact, NONE),
        // B9: cold GET serves identical bytes (gate 1st touch — no install)
        c(&[b"GET", b"s:bulk"], Exact, &[b"s:bulk"]),
        // gate 2nd touch — promotes, same bytes
        c(&[b"STRLEN", b"s:bulk"], Exact, NONE),
        // B9: cold GETRANGE serves the same window
        c(&[b"GETRANGE", b"s:bulk", b"0", b"9"], Exact, &[b"s:bulk"]),
        // cold-touched SETRANGE pages in and patches
        c(&[b"SETRANGE", b"s:bulk", b"10", b"XY"], Exact, NONE),
        // B9: RMW on cold pages in
        c(&[b"APPEND", b"s:bulk", b"tail"], Exact, &[b"s:bulk"]),
        // NX/XX on an existing (cold) key
        c(&[b"SET", b"s:bulk", b"nope", b"NX"], Exact, &[b"s:bulk"]), // B9: NX must fail on cold
        c(&[b"SET", b"s:bulk", b"xxv", b"XX"], Exact, NONE),          // B9: XX overwrites the stub
        // re-bigged so the WRONGTYPE specials run against a cold key
        c(&[b"SET", b"s:bulk", &[b'b'; 2500]], Exact, NONE),
        // WRONGTYPE against a cold string — refuse without resurrecting
        c(&[b"LPUSH", b"s:bulk", b"x"], Exact, &[b"s:bulk"]), // B9: WRONGTYPE, zero pread
        c(&[b"HSET", b"s:bulk", b"f", b"v"], Exact, NONE),    // B9: WRONGTYPE (still cold)
        // hashes (rows) — the RDS payload class
        c(&[b"HSET", b"h:row", b"name", b"ada", b"dept", b"eng", b"age", b"36"], Exact, NONE),
        c(&[b"HGET", b"h:row", b"name"], Exact, &[b"h:row"]), // B9: cold-row field read
        c(&[b"HSET", b"h:row", b"age", b"37"], Exact, NONE), // B9: cold-row write pages in, no shadow
        c(&[b"HGETALL", b"h:row"], Shape, NONE),             // field order is map order
        // B9: HGETALL straight off a cold row (gate serve)
        c(&[b"HGETALL", b"h:row"], Shape, &[b"h:row"]),
        c(&[b"HDEL", b"h:row", b"dept"], Exact, NONE), // pages back in
        c(&[b"HLEN", b"h:row"], Exact, NONE),
        // field TTL survival across demote/promote (B9 special)
        c(&[b"HSET", b"h:ttl", b"f1", b"v1", b"f2", b"v2"], Exact, NONE),
        c(&[b"HPEXPIRE", b"h:ttl", b"60000", b"FIELDS", b"1", b"f1"], Exact, NONE),
        // B9: after a demote round trip (inline hash forces too)
        c(&[b"HPERSIST", b"h:ttl", b"FIELDS", b"1", b"f1"], Exact, &[b"h:ttl"]),
        // TTL family on cold keys
        c(&[b"SET", b"t:k", &[b'b'; 1024]], Exact, NONE),
        c(&[b"PEXPIRE", b"t:k", b"600000"], Exact, &[b"t:k"]), // B9: EXPIRE on cold
        c(&[b"PERSIST", b"t:k"], Exact, NONE),
        c(&[b"TYPE", b"t:k"], Exact, NONE), // B9: TYPE from type_tag, zero pread
        // keyspace ops over cold keys
        c(&[b"EXISTS", b"s:bulk", b"h:row", b"absent"], Exact, NONE),
        c(&[b"RENAME", b"t:k", b"t:k2"], Exact, NONE), // B9: RENAME moves the stub, no read
        c(&[b"COPY", b"t:k2", b"t:k3"], Exact, NONE),  // materializes the copy, src stays cold
        c(&[b"DEL", b"t:k3"], Exact, &[b"t:k3"]),      // B9: DEL counts the cold key
        c(&[b"DBSIZE"], Exact, NONE),                  // B9: cold keys counted
        c(&[b"KEYS", b"*"], Shape, NONE),              // B9: cold keys visible (order = map order)
        c(&[b"SCAN", b"0"], Shape, NONE),              // B9: cold keys swept
        c(&[b"RANDOMKEY"], Shape, NONE),
        // memory-reporting: legitimately differs under tiering
        c(&[b"MEMORY", b"USAGE", b"s:bulk"], Shape, NONE),
        // collections stay hot in v1 — still must behave identically
        c(&[b"RPUSH", b"l:q", b"a", b"b"], Exact, NONE),
        c(&[b"LRANGE", b"l:q", b"0", b"-1"], Exact, NONE),
        c(&[b"ZADD", b"z:s", b"1", b"m1"], Exact, NONE),
        c(&[b"ZSCORE", b"z:s", b"m1"], Exact, NONE),
        // wipe-out
        c(&[b"FLUSHALL"], Exact, NONE), // B9: clears cold tier too
        c(&[b"DBSIZE"], Exact, NONE),
    ]
}

/// Drive both stores through the sequence; `demote` runs each case's
/// marker keys against the tiered side (`None` for the self-check).
fn run_pair(a: &Store, b: &Store, demote: bool) -> usize {
    let mut checked = 0;
    for case in cases() {
        let what =
            case.argv.iter().map(|x| String::from_utf8_lossy(x)).collect::<Vec<_>>().join(" ");
        if demote {
            for key in case.demote {
                assert!(
                    b.debug_force_demote(key),
                    "B9 marker must demote a warm spillable key: `{}` before `{what}`",
                    String::from_utf8_lossy(key)
                );
            }
        }
        let refs: Vec<&[u8]> = case.argv.iter().map(|v| v.as_slice()).collect();
        let ra = reply(a, &refs);
        let rb = reply(b, &refs);
        assert_match(&what, &case.cmp, &ra, &rb);
        checked += 1;
    }
    checked
}

/// Harness self-check: two identical untiered stores must agree on every
/// case — proves the sequence + compare machinery independent of tiering.
#[test]
fn transparency_harness_self_check_untiered() {
    let a = Store::open(Config::default().with_ttl_reaper_manual()).expect("open a");
    let b = Store::open(Config::default().with_ttl_reaper_manual()).expect("open b");
    let checked = run_pair(&a, &b, false);
    assert!(checked >= 30, "sequence unexpectedly short: {checked}");
}

/// The real suite (B9): tiered vs untiered, force-demoting at every
/// `// B9:` marker. Budget is huge so demotion happens ONLY at the
/// deterministic seam — never by watermark timing.
#[test]
fn transparency_tiered_vs_untiered() {
    let dir = kevy_tmpdir::TmpDir::new("tier-transparency");
    let a = Store::open(Config::default().with_ttl_reaper_manual()).expect("open untiered");
    let b = Store::open(
        Config::default()
            .with_ttl_reaper_manual()
            .with_persist(dir.path())
            .with_tier_budget(u64::MAX),
    )
    .expect("open tiered");
    let checked = run_pair(&a, &b, true);
    assert!(checked >= 30, "sequence unexpectedly short: {checked}");
    let (demotions, promotions) = b.tier_counters();
    // EXACT, not lower bounds (honesty audit 2026-07-25): all 10 markers
    // demote (each force is asserted), and the sequence deterministically
    // promotes exactly 5 times (the cold write-resolves — APPEND, XX SET
    // re-big, HSET on the cold row, HPERSIST on the cold hash — plus the
    // one second-touch gate promotion). Editing the sequence must re-derive
    // these numbers; that friction is the point.
    assert_eq!((demotions, promotions), (10, 5), "got {demotions}/{promotions}");
}

/// B12: demote/promote emit ZERO store-origin keyspace events and move
/// ONLY the tier counters — `evictions_total` stays untouched.
#[test]
fn demote_promote_move_tier_counters_only_and_emit_no_events() {
    let dir = kevy_tmpdir::TmpDir::new("tier-b12");
    let s = Store::open(
        Config::default()
            .with_ttl_reaper_manual()
            .with_persist(dir.path())
            .with_tier_budget(u64::MAX),
    )
    .expect("open tiered");
    s.set(b"cold:k", &[b'x'; 4096]).unwrap();
    // Capture every store-origin event kind, then drain the SET's `new`.
    s.with(|st| {
        st.set_notify_capture(true, true, true);
        st.take_notify_events();
    });
    assert!(s.debug_force_demote(b"cold:k"));
    // Two GETs: serve, then promote.
    assert_eq!(s.get(b"cold:k").unwrap().unwrap().len(), 4096);
    assert_eq!(s.get(b"cold:k").unwrap().unwrap().len(), 4096);
    let (demotions, promotions) = s.tier_counters();
    assert_eq!((demotions, promotions), (1, 1));
    s.with(|st| {
        assert_eq!(st.evictions_total(), 0, "demotion must not count as eviction");
        assert!(
            st.take_notify_events().is_empty(),
            "demote/promote must emit zero keyspace events"
        );
    });
}
