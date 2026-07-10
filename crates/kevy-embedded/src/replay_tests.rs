//! Tests for [`crate::replay`] (child module via `#[path]`).

use super::*;
use std::borrow::Cow;

fn argv(parts: &[&[u8]]) -> Argv {
    Argv::from(parts.iter().map(|p| p.to_vec()).collect::<Vec<_>>())
}

#[test]
fn set_get_through_apply() {
    let mut s = Store::new();
    apply(&mut s, &argv(&[b"SET", b"k", b"v"]));
    assert_eq!(s.get(b"k").unwrap(), Some(Cow::Borrowed(&b"v"[..])));
}

#[test]
fn all_basic_types_replay() {
    let mut s = Store::new();
    apply(&mut s, &argv(&[b"SET", b"str", b"hello"]));
    apply(&mut s, &argv(&[b"HSET", b"h", b"f1", b"v1", b"f2", b"v2"]));
    apply(&mut s, &argv(&[b"RPUSH", b"l", b"a", b"b", b"c"]));
    apply(&mut s, &argv(&[b"SADD", b"set", b"x", b"y"]));
    apply(&mut s, &argv(&[b"ZADD", b"z", b"1", b"a", b"2", b"b"]));
    apply(&mut s, &argv(&[b"PEXPIRE", b"str", b"60000"]));

    assert_eq!(s.dbsize(), 5);
    assert_eq!(s.type_of(b"str"), "string");
    assert_eq!(s.type_of(b"h"), "hash");
    assert_eq!(s.type_of(b"l"), "list");
    assert_eq!(s.type_of(b"set"), "set");
    assert_eq!(s.type_of(b"z"), "zset");
    assert!(s.pttl(b"str") > 50_000);
}

#[test]
fn unknown_verb_is_silently_ignored() {
    let mut s = Store::new();
    apply(&mut s, &argv(&[b"FROBNICATE", b"x"]));
    assert_eq!(s.dbsize(), 0);
}

#[test]
fn incrby_with_negative_replays() {
    let mut s = Store::new();
    apply(&mut s, &argv(&[b"INCRBY", b"n", b"5"]));
    apply(&mut s, &argv(&[b"INCRBY", b"n", b"3"]));
    apply(&mut s, &argv(&[b"DECRBY", b"n", b"4"]));
    assert_eq!(s.get(b"n").unwrap(), Some(Cow::Borrowed(&b"4"[..])));
}

/// A primary's frame stream can carry flag tokens — they must be
/// honored, never misparsed as scores (which would shift pairs).
#[test]
fn zadd_frame_with_flags_applies_conditionally() {
    let mut s = Store::new();
    apply(&mut s, &argv(&[b"ZADD", b"z", b"5", b"m"]));
    apply(&mut s, &argv(&[b"ZADD", b"z", b"GT", b"3", b"m"]));
    assert_eq!(s.zscore(b"z", b"m").unwrap(), Some(5.0));
    apply(&mut s, &argv(&[b"ZADD", b"z", b"GT", b"CH", b"7", b"m"]));
    assert_eq!(s.zscore(b"z", b"m").unwrap(), Some(7.0));
    apply(&mut s, &argv(&[b"ZADD", b"z", b"NX", b"1", b"m", b"2", b"n"]));
    assert_eq!(s.zscore(b"z", b"m").unwrap(), Some(7.0));
    assert_eq!(s.zscore(b"z", b"n").unwrap(), Some(2.0));
}

/// `ZADD … INCR delta member` is an increment, not an absolute
/// score.
#[test]
fn zadd_incr_frame_increments() {
    let mut s = Store::new();
    apply(&mut s, &argv(&[b"ZADD", b"z", b"5", b"m"]));
    apply(&mut s, &argv(&[b"ZADD", b"z", b"INCR", b"2", b"m"]));
    assert_eq!(s.zscore(b"z", b"m").unwrap(), Some(7.0));
    apply(&mut s, &argv(&[b"ZADD", b"z", b"GT", b"INCR", b"-3", b"m"]));
    assert_eq!(s.zscore(b"z", b"m").unwrap(), Some(7.0)); // vetoed
}
