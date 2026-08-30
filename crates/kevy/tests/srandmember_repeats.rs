//! `SRANDMEMBER key -count` — |count| members WITH repetition.
//!
//! Redis has had this form since 2.6. kevy rejected it outright with "value is
//! out of range, must be positive", so the one shape a caller uses to sample
//! WITH replacement was the one shape that errored.

use kevy_store::Store;

/// `SRANDMEMBER key -5` — five members WITH repetition. kevy used to reject
/// this outright; it is how you sample with replacement.
#[test]
fn srandmember_negative_count_allows_repeats() {
    let mut s = Store::new();
    let refs: Vec<&[u8]> = vec![b"a", b"b", b"c"];
    s.sadd(b"s", &refs).expect("sadd");
    let got = s.srandmember_with_repeats(b"s", 20).expect("with repeats");
    assert_eq!(got.len(), 20, "a negative count returns exactly |count|");
    assert_eq!(s.scard(b"s").expect("scard"), 3);
    let uniq: std::collections::HashSet<_> = got.iter().collect();
    assert!(uniq.len() <= 3, "cannot draw more than 3 distinct members from a 3-member set");
    // 20 draws from 3 members: the odds of no repeat are zero.
    assert!(uniq.len() < got.len(), "repetition never happened");
}

#[test]
fn a_missing_key_is_empty_not_an_error() {
    let mut s = Store::new();
    assert!(s.srandmember_with_repeats(b"nope", 3).expect("missing key").is_empty());
}
