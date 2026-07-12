//! SPOP and SRANDMEMBER return an ARBITRARY member. They did not.
//!
//! Both were `iter().take(count)` — the first members in hash-bucket order,
//! from a table with a fixed seed. For a given set that is the same answer every
//! call, in every process, forever. Handing work out, sampling a population,
//! assigning an A/B bucket: all three silently did the wrong thing while looking
//! like they worked, which is the worst way for anything to break.
//!
//! Every test here fails against that implementation. That is the point of them.

use kevy_store::Store;

fn set_of(n: usize) -> Store {
    let mut s = Store::new();
    let members: Vec<Vec<u8>> = (0..n).map(|i| format!("m{i:04}").into_bytes()).collect();
    let refs: Vec<&[u8]> = members.iter().map(Vec::as_slice).collect();
    s.sadd(b"s", &refs).expect("sadd");
    s
}

/// The old behaviour, stated as a test so it cannot come back: draw one member
/// from a hundred, a hundred times, and you must not get the same one every
/// time.
#[test]
fn srandmember_does_not_return_the_same_member_forever() {
    let mut seen = std::collections::HashSet::new();
    for _ in 0..100 {
        let mut s = set_of(100);
        let got = s.srandmember(b"s", 1).expect("srandmember");
        assert_eq!(got.len(), 1);
        seen.insert(got[0].clone());
    }
    assert!(
        seen.len() > 20,
        "100 draws from a 100-member set produced only {} distinct members — \
         this is the deterministic bug, not bad luck",
        seen.len()
    );
}

#[test]
fn spop_does_not_drain_in_the_same_order_every_time() {
    let mut firsts = std::collections::HashSet::new();
    for _ in 0..60 {
        let mut s = set_of(200);
        let got = s.spop(b"s", 1).expect("spop");
        firsts.insert(got[0].clone());
    }
    assert!(
        firsts.len() > 15,
        "60 pops from a 200-member set produced only {} distinct first members",
        firsts.len()
    );
}

/// Two stores that hold the same members must not agree on what "arbitrary"
/// means. Under the old code they always did, because the answer came from the
/// hash table's iteration order and nothing else.
#[test]
fn two_stores_with_identical_sets_disagree() {
    let mut a = set_of(256);
    let mut b = set_of(256);
    let mut same = 0;
    for _ in 0..20 {
        if a.spop(b"s", 1).expect("a") == b.spop(b"s", 1).expect("b") {
            same += 1;
        }
    }
    assert!(
        same < 12,
        "two stores agreed on {same}/20 pops — they are not drawing randomly"
    );
}

// ── the properties that must survive the change ─────────────────────────────

#[test]
fn spop_removes_what_it_returns_and_nothing_else() {
    let mut s = set_of(50);
    let got = s.spop(b"s", 7).expect("spop");
    assert_eq!(got.len(), 7);
    assert_eq!(s.scard(b"s").expect("scard"), 43);
    for m in &got {
        assert!(!s.sismember(b"s", m).expect("sismember"), "still present");
    }
}

#[test]
fn spop_returns_distinct_members() {
    let mut s = set_of(40);
    let got = s.spop(b"s", 40).expect("spop");
    let uniq: std::collections::HashSet<_> = got.iter().collect();
    assert_eq!(uniq.len(), got.len(), "SPOP returned a member twice");
    assert_eq!(got.len(), 40);
    assert_eq!(s.scard(b"s").expect("scard"), 0);
}

#[test]
fn spop_more_than_the_set_holds_empties_it() {
    let mut s = set_of(5);
    let got = s.spop(b"s", 500).expect("spop");
    assert_eq!(got.len(), 5);
    assert_eq!(s.scard(b"s").expect("scard"), 0);
}

#[test]
fn srandmember_returns_distinct_members_and_removes_none() {
    let mut s = set_of(100);
    let got = s.srandmember(b"s", 30).expect("srandmember");
    let uniq: std::collections::HashSet<_> = got.iter().collect();
    assert_eq!(uniq.len(), 30, "SRANDMEMBER repeated a member on a positive count");
    assert_eq!(s.scard(b"s").expect("scard"), 100, "SRANDMEMBER removed something");
}

/// The regime switch: asking for most of the set takes the copy-and-shuffle
/// path, asking for a few takes the probe path. Both must be correct.
#[test]
fn srandmember_across_both_regimes() {
    for count in [1usize, 3, 24, 25, 60, 99, 100, 101] {
        let mut s = set_of(100);
        let got = s.srandmember(b"s", count).expect("srandmember");
        let uniq: std::collections::HashSet<_> = got.iter().collect();
        assert_eq!(got.len(), count.min(100), "count={count}");
        assert_eq!(uniq.len(), got.len(), "count={count}: repeated a member");
    }
}


#[test]
fn empty_and_missing_sets_are_empty_not_an_error() {
    let mut s = Store::new();
    assert!(s.spop(b"nope", 3).expect("spop missing").is_empty());
    assert!(s.srandmember(b"nope", 3).expect("srand missing").is_empty());
}

/// The inline encoding (two members or fewer) takes a different branch. It has
/// to be random too — and it has to not lose members.
#[test]
fn the_inline_encoding_is_random_and_lossless() {
    let mut firsts = std::collections::HashSet::new();
    for _ in 0..40 {
        let mut s = Store::new();
        s.sadd(b"s", &[b"a".as_ref(), b"b".as_ref()]).expect("sadd");
        let got = s.spop(b"s", 1).expect("spop");
        assert_eq!(got.len(), 1);
        assert_eq!(s.scard(b"s").expect("scard"), 1);
        firsts.insert(got[0].clone());
    }
    assert_eq!(
        firsts.len(),
        2,
        "40 pops from {{a, b}} only ever returned {firsts:?} — the inline path is deterministic"
    );
}
