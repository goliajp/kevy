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
fn a_logged_set_keeps_its_relative_ttl_until_the_deadline_frame() {
    let mut s = Store::new();
    // a crash after this frame: the key must still expire
    apply(&mut s, &argv(&[b"SET", b"k", b"v", b"PX", b"60000"]));
    let ttl = s.pttl(b"k");
    assert!(ttl > 58_000 && ttl <= 60_000, "{ttl}");
    apply(&mut s, &argv(&[b"SET", b"s", b"v", b"NX", b"EX", b"60"]));
    assert!(s.pttl(b"s") > 58_000);
    // the deadline frame that follows wins, and a past one drops the key
    apply(&mut s, &argv(&[b"PEXPIREAT", b"k", b"1000"]));
    assert_eq!(s.get(b"k").unwrap(), None);
    // a frame the command refused when it ran, which a server logs too, is
    // refused again: the value is not set, with or without a TTL
    apply(&mut s, &argv(&[b"SET", b"bare", b"v", b"EX"]));
    apply(&mut s, &argv(&[b"SET", b"junk", b"v", b"PX", b"soon"]));
    assert_eq!((s.pttl(b"bare"), s.pttl(b"junk")), (-2, -2));
}

/// A frame applies to the state it was logged against, so a condition in
/// it holds again. A server records a vetoed `SET … NX` too; applied
/// without its condition, a replica would hand a held lock to the caller
/// that lost it.
#[test]
fn a_logged_condition_is_honored() {
    let mut s = Store::new();
    apply(&mut s, &argv(&[b"SET", b"lock", b"owner1", b"NX", b"PX", b"30000"]));
    apply(&mut s, &argv(&[b"SET", b"lock", b"owner2", b"NX", b"PX", b"30000"]));
    assert_eq!(s.get(b"lock").unwrap(), Some(Cow::Borrowed(&b"owner1"[..])));
    apply(&mut s, &argv(&[b"SET", b"gone", b"v", b"XX"]));
    assert_eq!(s.get(b"gone").unwrap(), None);
}

/// The server-only writes an embedded replica used to drop: its primary
/// logs them, so it has to apply them.
#[test]
fn server_only_writes_apply() {
    let mut s = Store::new();
    apply(&mut s, &argv(&[b"SETEX", b"a", b"100", b"1"]));
    apply(&mut s, &argv(&[b"PSETEX", b"b", b"100000", b"2"]));
    apply(&mut s, &argv(&[b"SETNX", b"c", b"3"]));
    apply(&mut s, &argv(&[b"MSET", b"d", b"4", b"e", b"5"]));
    apply(&mut s, &argv(&[b"UNLINK", b"e"]));
    apply(&mut s, &argv(&[b"HMSET", b"h", b"f", b"v"]));
    apply(&mut s, &argv(&[b"RPUSH", b"src", b"x", b"y"]));
    apply(&mut s, &argv(&[b"RPOPLPUSH", b"src", b"dst"]));
    apply(&mut s, &argv(&[b"LMOVE", b"src", b"dst", b"LEFT", b"LEFT"]));
    assert!(s.pttl(b"a") > 90_000 && s.pttl(b"b") > 90_000);
    assert_eq!(s.get(b"c").unwrap(), Some(Cow::Borrowed(&b"3"[..])));
    assert_eq!(s.get(b"d").unwrap(), Some(Cow::Borrowed(&b"4"[..])));
    assert_eq!(s.exists(&[b"e"]), 0);
    assert_eq!(s.hget(b"h", b"f").unwrap(), Some(&b"v"[..]));
    assert_eq!(s.lrange(b"dst", 0, -1).unwrap(), vec![b"x".to_vec(), b"y".to_vec()]);
}

/// Replay runs writes only; a read in a log applies nothing.
#[test]
fn a_read_in_the_log_is_skipped() {
    let mut s = Store::new();
    apply(&mut s, &argv(&[b"SET", b"k", b"v"]));
    apply(&mut s, &argv(&[b"GET", b"k"]));
    apply(&mut s, &argv(&[b"GETEX", b"k", b"EX", b"100"]));
    assert!(s.pttl(b"k") > 90_000, "GETEX is a write and moves the deadline");
    assert_eq!(s.dbsize(), 1);
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
