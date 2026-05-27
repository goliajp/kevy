//! Matrix-style coverage test for every Redis verb kevy handles in
//! [`kevy::dispatch`]. Each verb is exercised on at least one of:
//!   * a happy path,
//!   * an arity error (`wrong number of arguments`),
//!   * a wrong-type error (where the verb is type-checked),
//!   * a malformed-numeric error (where the verb parses an integer / float arg).
//!
//! The aim is to push `crates/kevy/src/cmd.rs` and `crates/kevy/src/dispatch.rs`
//! over the 80% line-coverage cement bar. The test calls
//! [`kevy::dispatch`] in-process against a single `KeyspaceStore` — no sockets,
//! no runtime — so it's order-independent within each #[test] but lets us
//! sequence happy → error in one body.
//!
//! Reproducer: `cargo test -p kevy --test cmd_matrix`.

use kevy::{Argv, KeyspaceStore, dispatch};

/// Build an `Argv` from `&[&[u8]]` argv pieces.
fn argv(parts: &[&[u8]]) -> Argv {
    Argv::from(parts.iter().map(|p| p.to_vec()).collect::<Vec<_>>())
}

/// Convenience: run one command against `store` and return the reply bytes.
fn run(store: &mut KeyspaceStore, parts: &[&[u8]]) -> Vec<u8> {
    dispatch(store, &argv(parts))
}

/// Assert reply equals `want`.
fn assert_eq_reply(reply: &[u8], want: &[u8], label: &str) {
    assert_eq!(
        reply,
        want,
        "{label}: got {:?}, want {:?}",
        std::str::from_utf8(reply).unwrap_or("<binary>"),
        std::str::from_utf8(want).unwrap_or("<binary>"),
    );
}

/// Assert reply starts with `prefix` (useful for `-ERR ...` whose exact text
/// we don't want to lock in).
fn assert_starts(reply: &[u8], prefix: &[u8], label: &str) {
    assert!(
        reply.starts_with(prefix),
        "{label}: expected prefix {:?}, got {:?}",
        std::str::from_utf8(prefix).unwrap_or("<binary>"),
        std::str::from_utf8(reply).unwrap_or("<binary>"),
    );
}

// ─────────────────────────── connection / introspection ─────────────────────

#[test]
fn conn_and_introspection() {
    let mut s = KeyspaceStore::new();

    // PING (0,1,2 args)
    assert_eq_reply(&run(&mut s, &[b"PING"]), b"+PONG\r\n", "PING/0");
    assert_eq_reply(
        &run(&mut s, &[b"PING", b"hi"]),
        b"$2\r\nhi\r\n",
        "PING/1",
    );
    assert_starts(&run(&mut s, &[b"PING", b"a", b"b"]), b"-ERR", "PING/2");

    // ECHO
    assert_eq_reply(
        &run(&mut s, &[b"ECHO", b"hello"]),
        b"$5\r\nhello\r\n",
        "ECHO/1",
    );
    assert_starts(&run(&mut s, &[b"ECHO"]), b"-ERR", "ECHO/0");

    // COMMAND
    assert_eq_reply(&run(&mut s, &[b"COMMAND"]), b"*0\r\n", "COMMAND");
    // QUIT
    assert_eq_reply(&run(&mut s, &[b"QUIT"]), b"+OK\r\n", "QUIT");
    // HELLO (multi-line map reply — just check it's nonempty)
    assert!(!run(&mut s, &[b"HELLO"]).is_empty(), "HELLO");

    // CONFIG GET / SET: Wave 1 replaced the prior tolerant stubs with the
    // real Config-backed handler. GET against an unknown key still returns
    // an empty array; SET is read-only in v1.0 (errors with a v1.x Wave 2
    // pointer).
    assert_eq_reply(
        &run(&mut s, &[b"CONFIG", b"GET", b"nonexistent-setting"]),
        b"*0\r\n",
        "CONFIG GET unknown",
    );
    assert_starts(
        &run(&mut s, &[b"CONFIG", b"SET", b"k", b"v"]),
        b"-ERR",
        "CONFIG SET",
    );
    // Bare `CONFIG` (no subcommand) is now wrong-args error rather than +OK
    // — the tolerant stub is gone, the real dispatcher requires a subcommand.
    assert_starts(&run(&mut s, &[b"CONFIG"]), b"-ERR", "CONFIG bare");

    // Empty cmd / unknown verb.
    let empty_argv = Argv::default();
    assert_starts(&dispatch(&mut s, &empty_argv), b"-ERR", "empty argv");
    assert_starts(
        &run(&mut s, &[b"NOPE_NOT_A_CMD"]),
        b"-ERR unknown command",
        "unknown verb",
    );
}

// ─────────────────────────── string commands ────────────────────────────────

#[test]
fn string_set_get_family() {
    let mut s = KeyspaceStore::new();

    // SET / GET happy + arity-err.
    assert_eq_reply(&run(&mut s, &[b"SET", b"k", b"v"]), b"+OK\r\n", "SET");
    assert_eq_reply(&run(&mut s, &[b"GET", b"k"]), b"$1\r\nv\r\n", "GET hit");
    assert_eq_reply(&run(&mut s, &[b"GET", b"nope"]), b"$-1\r\n", "GET miss");
    assert_starts(&run(&mut s, &[b"GET"]), b"-ERR", "GET/0");
    assert_starts(&run(&mut s, &[b"SET", b"k"]), b"-ERR", "SET/1");

    // SET with EX / PX / NX / XX / syntax err.
    assert_eq_reply(
        &run(&mut s, &[b"SET", b"k", b"v", b"EX", b"100"]),
        b"+OK\r\n",
        "SET EX",
    );
    assert_eq_reply(
        &run(&mut s, &[b"SET", b"k", b"v", b"PX", b"123"]),
        b"+OK\r\n",
        "SET PX",
    );
    assert_starts(
        &run(&mut s, &[b"SET", b"k", b"v", b"EX"]),
        b"-ERR",
        "SET EX missing arg",
    );
    assert_starts(
        &run(&mut s, &[b"SET", b"k", b"v", b"EX", b"abc"]),
        b"-ERR",
        "SET EX bad int",
    );
    assert_starts(
        &run(&mut s, &[b"SET", b"k", b"v", b"BOGUS"]),
        b"-ERR",
        "SET unknown opt",
    );
    assert_starts(
        &run(&mut s, &[b"SET", b"k", b"v", b"NX", b"XX"]),
        b"-ERR",
        "SET NX+XX",
    );
    // NX skip & XX skip paths.
    assert_eq_reply(
        &run(&mut s, &[b"SET", b"k", b"v2", b"NX"]),
        b"$-1\r\n",
        "SET NX skip",
    );
    let _ = run(&mut s, &[b"DEL", b"missing_xx"]);
    assert_eq_reply(
        &run(&mut s, &[b"SET", b"missing_xx", b"v", b"XX"]),
        b"$-1\r\n",
        "SET XX skip",
    );

    // SETNX (own verb)
    assert_eq_reply(&run(&mut s, &[b"SETNX", b"nx", b"v"]), b":1\r\n", "SETNX new");
    assert_eq_reply(
        &run(&mut s, &[b"SETNX", b"nx", b"w"]),
        b":0\r\n",
        "SETNX exist",
    );
    assert_starts(&run(&mut s, &[b"SETNX", b"x"]), b"-ERR", "SETNX/1");

    // SETEX / PSETEX
    assert_eq_reply(
        &run(&mut s, &[b"SETEX", b"se", b"100", b"v"]),
        b"+OK\r\n",
        "SETEX",
    );
    assert_eq_reply(
        &run(&mut s, &[b"PSETEX", b"pse", b"1000", b"v"]),
        b"+OK\r\n",
        "PSETEX",
    );
    assert_starts(
        &run(&mut s, &[b"SETEX", b"k", b"v"]),
        b"-ERR",
        "SETEX/2",
    );
    assert_starts(
        &run(&mut s, &[b"SETEX", b"k", b"abc", b"v"]),
        b"-ERR",
        "SETEX bad ttl",
    );
    assert_starts(
        &run(&mut s, &[b"SETEX", b"k", b"0", b"v"]),
        b"-ERR",
        "SETEX zero ttl",
    );

    // APPEND
    let _ = run(&mut s, &[b"DEL", b"ap"]);
    assert_eq_reply(&run(&mut s, &[b"APPEND", b"ap", b"foo"]), b":3\r\n", "APPEND new");
    assert_eq_reply(
        &run(&mut s, &[b"APPEND", b"ap", b"bar"]),
        b":6\r\n",
        "APPEND ext",
    );
    assert_starts(&run(&mut s, &[b"APPEND", b"ap"]), b"-ERR", "APPEND/1");

    // STRLEN happy + miss + arity-err.
    assert_eq_reply(&run(&mut s, &[b"STRLEN", b"ap"]), b":6\r\n", "STRLEN hit");
    assert_eq_reply(&run(&mut s, &[b"STRLEN", b"absent"]), b":0\r\n", "STRLEN miss");
    assert_starts(&run(&mut s, &[b"STRLEN"]), b"-ERR", "STRLEN/0");

    // GETSET / GETDEL
    let _ = run(&mut s, &[b"SET", b"gs", b"a"]);
    assert_eq_reply(
        &run(&mut s, &[b"GETSET", b"gs", b"b"]),
        b"$1\r\na\r\n",
        "GETSET",
    );
    assert_eq_reply(
        &run(&mut s, &[b"GETSET", b"never", b"v"]),
        b"$-1\r\n",
        "GETSET miss",
    );
    assert_starts(&run(&mut s, &[b"GETSET", b"only"]), b"-ERR", "GETSET/1");

    let _ = run(&mut s, &[b"SET", b"gd", b"x"]);
    assert_eq_reply(
        &run(&mut s, &[b"GETDEL", b"gd"]),
        b"$1\r\nx\r\n",
        "GETDEL",
    );
    assert_eq_reply(&run(&mut s, &[b"GETDEL", b"gd"]), b"$-1\r\n", "GETDEL miss");
    assert_starts(&run(&mut s, &[b"GETDEL"]), b"-ERR", "GETDEL/0");

    // INCRBYFLOAT happy + bad float.
    let _ = run(&mut s, &[b"DEL", b"f"]);
    assert!(
        run(&mut s, &[b"INCRBYFLOAT", b"f", b"1.5"]).starts_with(b"$"),
        "INCRBYFLOAT initial"
    );
    assert_starts(
        &run(&mut s, &[b"INCRBYFLOAT", b"f", b"NaN"]),
        b"-ERR",
        "INCRBYFLOAT NaN",
    );
    assert_starts(
        &run(&mut s, &[b"INCRBYFLOAT", b"f"]),
        b"-ERR",
        "INCRBYFLOAT/1",
    );
}

#[test]
fn string_counters_and_wrongtype() {
    let mut s = KeyspaceStore::new();

    // INCR / DECR / INCRBY / DECRBY happy + bad-int.
    assert_eq_reply(&run(&mut s, &[b"INCR", b"n"]), b":1\r\n", "INCR new");
    assert_eq_reply(&run(&mut s, &[b"INCR", b"n"]), b":2\r\n", "INCR inc");
    assert_eq_reply(&run(&mut s, &[b"DECR", b"n"]), b":1\r\n", "DECR");
    assert_eq_reply(&run(&mut s, &[b"INCRBY", b"n", b"10"]), b":11\r\n", "INCRBY");
    assert_eq_reply(&run(&mut s, &[b"DECRBY", b"n", b"4"]), b":7\r\n", "DECRBY");
    assert_starts(
        &run(&mut s, &[b"INCRBY", b"n", b"abc"]),
        b"-ERR",
        "INCRBY bad int",
    );
    assert_starts(&run(&mut s, &[b"INCRBY", b"n"]), b"-ERR", "INCRBY/1");
    assert_starts(&run(&mut s, &[b"INCR"]), b"-ERR", "INCR/0");

    // Wrong-type: counter on a non-int-shaped string.
    let _ = run(&mut s, &[b"SET", b"str", b"abc"]);
    assert_starts(&run(&mut s, &[b"INCR", b"str"]), b"-ERR", "INCR str");
}

// ─────────────────────────── hash commands ──────────────────────────────────

#[test]
fn hash_full_surface() {
    let mut s = KeyspaceStore::new();

    // HSET single + multi + arity-err.
    assert_eq_reply(
        &run(&mut s, &[b"HSET", b"h", b"f1", b"v1"]),
        b":1\r\n",
        "HSET new",
    );
    assert_eq_reply(
        &run(&mut s, &[b"HSET", b"h", b"f1", b"v1b", b"f2", b"v2"]),
        b":1\r\n",
        "HSET upd+new",
    );
    assert_starts(
        &run(&mut s, &[b"HSET", b"h", b"f1"]),
        b"-ERR",
        "HSET odd pairs",
    );

    // HSETNX twice + arity-err.
    assert_eq_reply(
        &run(&mut s, &[b"HSETNX", b"h", b"f3", b"v3"]),
        b":1\r\n",
        "HSETNX new",
    );
    assert_eq_reply(
        &run(&mut s, &[b"HSETNX", b"h", b"f3", b"v3b"]),
        b":0\r\n",
        "HSETNX exist",
    );
    assert_starts(&run(&mut s, &[b"HSETNX", b"h", b"f3"]), b"-ERR", "HSETNX/2");

    // HGET / HEXISTS / HLEN happy + arity err.
    assert_eq_reply(
        &run(&mut s, &[b"HGET", b"h", b"f1"]),
        b"$3\r\nv1b\r\n",
        "HGET hit",
    );
    assert_eq_reply(
        &run(&mut s, &[b"HGET", b"h", b"absent"]),
        b"$-1\r\n",
        "HGET miss field",
    );
    assert_starts(&run(&mut s, &[b"HGET", b"h"]), b"-ERR", "HGET/1");
    assert_eq_reply(&run(&mut s, &[b"HEXISTS", b"h", b"f1"]), b":1\r\n", "HEXISTS");
    assert_eq_reply(
        &run(&mut s, &[b"HEXISTS", b"h", b"x"]),
        b":0\r\n",
        "HEXISTS miss",
    );
    assert_starts(&run(&mut s, &[b"HEXISTS", b"h"]), b"-ERR", "HEXISTS/1");
    assert!(run(&mut s, &[b"HLEN", b"h"]).starts_with(b":"), "HLEN hit");
    assert_eq_reply(&run(&mut s, &[b"HLEN", b"absent"]), b":0\r\n", "HLEN miss");
    assert_starts(&run(&mut s, &[b"HLEN"]), b"-ERR", "HLEN/0");

    // HKEYS / HVALS / HGETALL — just non-empty / arity-err.
    assert!(run(&mut s, &[b"HKEYS", b"h"]).starts_with(b"*"), "HKEYS");
    assert!(run(&mut s, &[b"HVALS", b"h"]).starts_with(b"*"), "HVALS");
    assert!(
        run(&mut s, &[b"HGETALL", b"h"]).starts_with(b"*"),
        "HGETALL"
    );
    assert_eq_reply(&run(&mut s, &[b"HKEYS", b"absent"]), b"*0\r\n", "HKEYS miss");
    assert_starts(&run(&mut s, &[b"HKEYS"]), b"-ERR", "HKEYS/0");
    assert_starts(&run(&mut s, &[b"HVALS"]), b"-ERR", "HVALS/0");
    assert_starts(&run(&mut s, &[b"HGETALL"]), b"-ERR", "HGETALL/0");

    // HMGET happy + arity.
    let r = run(&mut s, &[b"HMGET", b"h", b"f1", b"absent"]);
    assert!(r.starts_with(b"*2"), "HMGET");
    assert_starts(&run(&mut s, &[b"HMGET", b"h"]), b"-ERR", "HMGET/1");

    // HINCRBY happy + bad-int + arity.
    assert_eq_reply(
        &run(&mut s, &[b"HINCRBY", b"h", b"counter", b"5"]),
        b":5\r\n",
        "HINCRBY new",
    );
    assert_eq_reply(
        &run(&mut s, &[b"HINCRBY", b"h", b"counter", b"-2"]),
        b":3\r\n",
        "HINCRBY dec",
    );
    assert_starts(
        &run(&mut s, &[b"HINCRBY", b"h", b"counter", b"oops"]),
        b"-ERR",
        "HINCRBY bad",
    );
    assert_starts(
        &run(&mut s, &[b"HINCRBY", b"h", b"counter"]),
        b"-ERR",
        "HINCRBY/2",
    );

    // HDEL happy + arity.
    assert!(
        run(&mut s, &[b"HDEL", b"h", b"f1", b"absent"]).starts_with(b":"),
        "HDEL",
    );
    assert_starts(&run(&mut s, &[b"HDEL", b"h"]), b"-ERR", "HDEL/1");

    // Wrong-type: hash command on a string key.
    let _ = run(&mut s, &[b"SET", b"str", b"x"]);
    assert_starts(
        &run(&mut s, &[b"HSET", b"str", b"f", b"v"]),
        b"-WRONGTYPE",
        "HSET on string",
    );
    assert_starts(
        &run(&mut s, &[b"HGET", b"str", b"f"]),
        b"-WRONGTYPE",
        "HGET on string",
    );
}

// ─────────────────────────── list commands ──────────────────────────────────

#[test]
fn list_full_surface() {
    let mut s = KeyspaceStore::new();

    // LPUSH / RPUSH happy + arity.
    assert_eq_reply(
        &run(&mut s, &[b"RPUSH", b"l", b"a", b"b", b"c"]),
        b":3\r\n",
        "RPUSH",
    );
    assert_eq_reply(
        &run(&mut s, &[b"LPUSH", b"l", b"z"]),
        b":4\r\n",
        "LPUSH",
    );
    assert_starts(&run(&mut s, &[b"RPUSH", b"l"]), b"-ERR", "RPUSH/1");
    assert_starts(&run(&mut s, &[b"LPUSH", b"l"]), b"-ERR", "LPUSH/1");

    // LLEN / LINDEX / LRANGE happy + bad-int + arity.
    assert_eq_reply(&run(&mut s, &[b"LLEN", b"l"]), b":4\r\n", "LLEN");
    assert_eq_reply(&run(&mut s, &[b"LLEN", b"absent"]), b":0\r\n", "LLEN miss");
    assert_starts(&run(&mut s, &[b"LLEN"]), b"-ERR", "LLEN/0");
    assert_eq_reply(
        &run(&mut s, &[b"LINDEX", b"l", b"0"]),
        b"$1\r\nz\r\n",
        "LINDEX hit",
    );
    assert_eq_reply(
        &run(&mut s, &[b"LINDEX", b"l", b"99"]),
        b"$-1\r\n",
        "LINDEX miss",
    );
    assert_starts(
        &run(&mut s, &[b"LINDEX", b"l", b"abc"]),
        b"-ERR",
        "LINDEX bad int",
    );
    assert_starts(&run(&mut s, &[b"LINDEX", b"l"]), b"-ERR", "LINDEX/1");
    assert!(
        run(&mut s, &[b"LRANGE", b"l", b"0", b"-1"]).starts_with(b"*"),
        "LRANGE",
    );
    assert_starts(
        &run(&mut s, &[b"LRANGE", b"l", b"a", b"b"]),
        b"-ERR",
        "LRANGE bad int",
    );
    assert_starts(&run(&mut s, &[b"LRANGE", b"l"]), b"-ERR", "LRANGE/1");

    // LSET happy + miss + bad-int + arity.
    assert_eq_reply(
        &run(&mut s, &[b"LSET", b"l", b"0", b"Z"]),
        b"+OK\r\n",
        "LSET",
    );
    assert_starts(
        &run(&mut s, &[b"LSET", b"l", b"abc", b"v"]),
        b"-ERR",
        "LSET bad int",
    );
    assert_starts(&run(&mut s, &[b"LSET", b"l"]), b"-ERR", "LSET/1");
    assert_starts(
        &run(&mut s, &[b"LSET", b"l", b"99", b"v"]),
        b"-ERR",
        "LSET oor",
    );

    // LREM / LTRIM happy + bad-int + arity.
    assert!(
        run(&mut s, &[b"LREM", b"l", b"0", b"a"]).starts_with(b":"),
        "LREM",
    );
    assert_starts(
        &run(&mut s, &[b"LREM", b"l", b"abc", b"x"]),
        b"-ERR",
        "LREM bad int",
    );
    assert_starts(&run(&mut s, &[b"LREM", b"l"]), b"-ERR", "LREM/1");

    assert_eq_reply(
        &run(&mut s, &[b"LTRIM", b"l", b"0", b"-1"]),
        b"+OK\r\n",
        "LTRIM",
    );
    assert_starts(
        &run(&mut s, &[b"LTRIM", b"l", b"a", b"b"]),
        b"-ERR",
        "LTRIM bad int",
    );
    assert_starts(&run(&mut s, &[b"LTRIM", b"l"]), b"-ERR", "LTRIM/1");

    // LPOP / RPOP both forms.
    assert!(run(&mut s, &[b"LPOP", b"l"]).starts_with(b"$"), "LPOP single");
    assert!(run(&mut s, &[b"RPOP", b"l"]).starts_with(b"$"), "RPOP single");
    assert!(
        run(&mut s, &[b"LPOP", b"l", b"2"]).starts_with(b"*"),
        "LPOP count",
    );
    // Count form on empty list returns nil array.
    assert_eq_reply(
        &run(&mut s, &[b"LPOP", b"absent", b"5"]),
        b"*-1\r\n",
        "LPOP count miss",
    );
    assert_starts(
        &run(&mut s, &[b"LPOP", b"l", b"abc"]),
        b"-ERR",
        "LPOP bad int",
    );
    assert_starts(
        &run(&mut s, &[b"LPOP", b"l", b"-1"]),
        b"-ERR",
        "LPOP neg",
    );
    assert_starts(&run(&mut s, &[b"LPOP"]), b"-ERR", "LPOP/0");

    // Wrong-type: list command on string key.
    let _ = run(&mut s, &[b"SET", b"str", b"x"]);
    assert_starts(
        &run(&mut s, &[b"LPUSH", b"str", b"v"]),
        b"-WRONGTYPE",
        "LPUSH on string",
    );
}

// ─────────────────────────── set commands ───────────────────────────────────

#[test]
fn set_full_surface() {
    let mut s = KeyspaceStore::new();

    assert!(
        run(&mut s, &[b"SADD", b"S", b"a", b"b", b"c"]).starts_with(b":"),
        "SADD",
    );
    assert_starts(&run(&mut s, &[b"SADD", b"S"]), b"-ERR", "SADD/1");

    assert!(run(&mut s, &[b"SCARD", b"S"]).starts_with(b":"), "SCARD");
    assert_eq_reply(&run(&mut s, &[b"SCARD", b"absent"]), b":0\r\n", "SCARD miss");
    assert_starts(&run(&mut s, &[b"SCARD"]), b"-ERR", "SCARD/0");

    assert_eq_reply(
        &run(&mut s, &[b"SISMEMBER", b"S", b"a"]),
        b":1\r\n",
        "SISMEMBER hit",
    );
    assert_eq_reply(
        &run(&mut s, &[b"SISMEMBER", b"S", b"z"]),
        b":0\r\n",
        "SISMEMBER miss",
    );
    assert_starts(&run(&mut s, &[b"SISMEMBER", b"S"]), b"-ERR", "SISMEMBER/1");

    assert!(run(&mut s, &[b"SMEMBERS", b"S"]).starts_with(b"*"), "SMEMBERS");
    assert_starts(&run(&mut s, &[b"SMEMBERS"]), b"-ERR", "SMEMBERS/0");

    assert!(run(&mut s, &[b"SREM", b"S", b"a"]).starts_with(b":"), "SREM");
    assert_starts(&run(&mut s, &[b"SREM", b"S"]), b"-ERR", "SREM/1");

    // SPOP / SRANDMEMBER both forms.
    let _ = run(&mut s, &[b"SADD", b"S", b"x", b"y", b"z"]);
    assert!(run(&mut s, &[b"SPOP", b"S"]).starts_with(b"$"), "SPOP single");
    assert!(
        run(&mut s, &[b"SPOP", b"S", b"2"]).starts_with(b"*"),
        "SPOP count",
    );
    assert!(
        run(&mut s, &[b"SRANDMEMBER", b"S"]).starts_with(b"$") ||
        run(&mut s, &[b"SRANDMEMBER", b"S"]).starts_with(b"$-1"),
        "SRANDMEMBER single",
    );
    assert!(
        run(&mut s, &[b"SRANDMEMBER", b"S", b"2"]).starts_with(b"*"),
        "SRANDMEMBER count",
    );
    assert_starts(
        &run(&mut s, &[b"SPOP", b"S", b"-1"]),
        b"-ERR",
        "SPOP neg",
    );
    assert_starts(
        &run(&mut s, &[b"SPOP", b"S", b"abc"]),
        b"-ERR",
        "SPOP bad int",
    );
    assert_starts(&run(&mut s, &[b"SPOP"]), b"-ERR", "SPOP/0");
    assert_starts(&run(&mut s, &[b"SRANDMEMBER"]), b"-ERR", "SRANDMEMBER/0");

    // Wrong-type: set command on a string key.
    let _ = run(&mut s, &[b"SET", b"str", b"x"]);
    assert_starts(
        &run(&mut s, &[b"SADD", b"str", b"v"]),
        b"-WRONGTYPE",
        "SADD on string",
    );
}

// ─────────────────────────── sorted-set commands ────────────────────────────

#[test]
fn zset_full_surface() {
    let mut s = KeyspaceStore::new();

    // ZADD happy + bad-float + arity.
    assert_eq_reply(
        &run(&mut s, &[b"ZADD", b"z", b"1", b"a", b"2", b"b", b"3", b"c"]),
        b":3\r\n",
        "ZADD",
    );
    assert_starts(
        &run(&mut s, &[b"ZADD", b"z", b"x", b"a"]),
        b"-ERR",
        "ZADD bad float",
    );
    assert_starts(
        &run(&mut s, &[b"ZADD", b"z", b"1"]),
        b"-ERR",
        "ZADD/2 odd",
    );
    assert_starts(&run(&mut s, &[b"ZADD", b"z"]), b"-ERR", "ZADD/1");

    // ZSCORE / ZCARD / ZRANK / ZINCRBY happy + arity.
    assert!(run(&mut s, &[b"ZSCORE", b"z", b"a"]).starts_with(b"$"), "ZSCORE");
    assert_eq_reply(
        &run(&mut s, &[b"ZSCORE", b"z", b"absent"]),
        b"$-1\r\n",
        "ZSCORE miss",
    );
    assert_starts(&run(&mut s, &[b"ZSCORE", b"z"]), b"-ERR", "ZSCORE/1");

    assert!(run(&mut s, &[b"ZCARD", b"z"]).starts_with(b":"), "ZCARD");
    assert_starts(&run(&mut s, &[b"ZCARD"]), b"-ERR", "ZCARD/0");

    assert!(run(&mut s, &[b"ZRANK", b"z", b"a"]).starts_with(b":"), "ZRANK");
    assert_eq_reply(
        &run(&mut s, &[b"ZRANK", b"z", b"absent"]),
        b"$-1\r\n",
        "ZRANK miss",
    );
    assert_starts(&run(&mut s, &[b"ZRANK", b"z"]), b"-ERR", "ZRANK/1");

    assert!(
        run(&mut s, &[b"ZINCRBY", b"z", b"1.5", b"a"]).starts_with(b"$"),
        "ZINCRBY",
    );
    assert_starts(
        &run(&mut s, &[b"ZINCRBY", b"z", b"oops", b"a"]),
        b"-ERR",
        "ZINCRBY bad float",
    );
    assert_starts(&run(&mut s, &[b"ZINCRBY", b"z", b"1"]), b"-ERR", "ZINCRBY/2");

    // ZRANGE: rank + WITHSCORES + bad arg.
    assert!(
        run(&mut s, &[b"ZRANGE", b"z", b"0", b"-1"]).starts_with(b"*"),
        "ZRANGE",
    );
    assert!(
        run(&mut s, &[b"ZRANGE", b"z", b"0", b"-1", b"WITHSCORES"]).starts_with(b"*"),
        "ZRANGE WS",
    );
    assert_starts(
        &run(&mut s, &[b"ZRANGE", b"z", b"0", b"-1", b"BOGUS"]),
        b"-ERR",
        "ZRANGE bad opt",
    );
    assert_starts(
        &run(&mut s, &[b"ZRANGE", b"z", b"a", b"b"]),
        b"-ERR",
        "ZRANGE bad int",
    );
    assert_starts(&run(&mut s, &[b"ZRANGE", b"z"]), b"-ERR", "ZRANGE/1");
    assert_starts(
        &run(&mut s, &[b"ZRANGE", b"z", b"0", b"-1", b"WITHSCORES", b"X"]),
        b"-ERR",
        "ZRANGE/5",
    );

    // ZRANGEBYSCORE w/ exclusive bound + WITHSCORES + bad bound.
    assert!(
        run(&mut s, &[b"ZRANGEBYSCORE", b"z", b"-inf", b"+inf"]).starts_with(b"*"),
        "ZRANGEBYSCORE",
    );
    assert!(
        run(
            &mut s,
            &[b"ZRANGEBYSCORE", b"z", b"(1", b"+inf", b"WITHSCORES"],
        )
        .starts_with(b"*"),
        "ZRANGEBYSCORE excl WS",
    );
    assert_starts(
        &run(&mut s, &[b"ZRANGEBYSCORE", b"z", b"foo", b"bar"]),
        b"-ERR",
        "ZRANGEBYSCORE bad bound",
    );
    assert_starts(
        &run(&mut s, &[b"ZRANGEBYSCORE", b"z", b"0", b"1", b"BOGUS"]),
        b"-ERR",
        "ZRANGEBYSCORE bad opt",
    );
    assert_starts(&run(&mut s, &[b"ZRANGEBYSCORE", b"z"]), b"-ERR", "ZRBS/1");

    // ZCOUNT happy + bad bound + arity.
    assert!(
        run(&mut s, &[b"ZCOUNT", b"z", b"0", b"10"]).starts_with(b":"),
        "ZCOUNT",
    );
    assert_starts(
        &run(&mut s, &[b"ZCOUNT", b"z", b"x", b"y"]),
        b"-ERR",
        "ZCOUNT bad",
    );
    assert_starts(&run(&mut s, &[b"ZCOUNT", b"z", b"0"]), b"-ERR", "ZCOUNT/2");

    // ZREM
    assert!(
        run(&mut s, &[b"ZREM", b"z", b"a", b"b"]).starts_with(b":"),
        "ZREM",
    );
    assert_starts(&run(&mut s, &[b"ZREM", b"z"]), b"-ERR", "ZREM/1");

    // Wrong-type: zset command on a string key.
    let _ = run(&mut s, &[b"SET", b"str", b"x"]);
    assert_starts(
        &run(&mut s, &[b"ZADD", b"str", b"1", b"a"]),
        b"-WRONGTYPE",
        "ZADD on string",
    );
}

// ─────────────────────────── generic / key ops ──────────────────────────────

#[test]
fn generic_keyspace() {
    let mut s = KeyspaceStore::new();

    let _ = run(&mut s, &[b"SET", b"a", b"1"]);
    let _ = run(&mut s, &[b"SET", b"b", b"2"]);

    // DEL / EXISTS happy + arity.
    assert_eq_reply(&run(&mut s, &[b"DEL", b"a"]), b":1\r\n", "DEL");
    assert_eq_reply(
        &run(&mut s, &[b"EXISTS", b"b", b"absent"]),
        b":1\r\n",
        "EXISTS multi",
    );
    assert_starts(&run(&mut s, &[b"DEL"]), b"-ERR", "DEL/0");
    assert_starts(&run(&mut s, &[b"EXISTS"]), b"-ERR", "EXISTS/0");

    // EXPIRE / PEXPIRE / PERSIST / TTL / PTTL.
    assert_eq_reply(
        &run(&mut s, &[b"EXPIRE", b"b", b"100"]),
        b":1\r\n",
        "EXPIRE",
    );
    assert_eq_reply(
        &run(&mut s, &[b"PEXPIRE", b"b", b"50000"]),
        b":1\r\n",
        "PEXPIRE",
    );
    assert!(
        run(&mut s, &[b"EXPIRE", b"absent", b"100"]).starts_with(b":0"),
        "EXPIRE miss",
    );
    assert_starts(
        &run(&mut s, &[b"EXPIRE", b"b", b"abc"]),
        b"-ERR",
        "EXPIRE bad int",
    );
    assert_starts(&run(&mut s, &[b"EXPIRE", b"b"]), b"-ERR", "EXPIRE/1");

    assert!(run(&mut s, &[b"TTL", b"b"]).starts_with(b":"), "TTL");
    assert!(run(&mut s, &[b"PTTL", b"b"]).starts_with(b":"), "PTTL");
    assert_eq_reply(&run(&mut s, &[b"TTL", b"absent"]), b":-2\r\n", "TTL miss");
    assert_eq_reply(&run(&mut s, &[b"PTTL", b"absent"]), b":-2\r\n", "PTTL miss");
    assert_starts(&run(&mut s, &[b"TTL"]), b"-ERR", "TTL/0");
    assert_starts(&run(&mut s, &[b"PTTL"]), b"-ERR", "PTTL/0");

    assert_eq_reply(&run(&mut s, &[b"PERSIST", b"b"]), b":1\r\n", "PERSIST");
    assert_eq_reply(&run(&mut s, &[b"PERSIST", b"absent"]), b":0\r\n", "PERSIST miss");
    assert_starts(&run(&mut s, &[b"PERSIST"]), b"-ERR", "PERSIST/0");

    // TYPE
    assert_eq_reply(&run(&mut s, &[b"TYPE", b"b"]), b"+string\r\n", "TYPE str");
    assert_eq_reply(
        &run(&mut s, &[b"TYPE", b"absent"]),
        b"+none\r\n",
        "TYPE miss",
    );
    assert_starts(&run(&mut s, &[b"TYPE"]), b"-ERR", "TYPE/0");

    // DBSIZE / FLUSHDB / FLUSHALL.
    assert!(run(&mut s, &[b"DBSIZE"]).starts_with(b":"), "DBSIZE");
    assert_eq_reply(&run(&mut s, &[b"FLUSHDB"]), b"+OK\r\n", "FLUSHDB");
    let _ = run(&mut s, &[b"SET", b"x", b"y"]);
    assert_eq_reply(&run(&mut s, &[b"FLUSHALL"]), b"+OK\r\n", "FLUSHALL");
}

// ─────────────────────────── multikey & pub/sub stubs ───────────────────────

// These verbs are normally cross-shard at the route layer and only reach
// `dispatch_into` when malformed; here we directly call dispatch to exercise
// the arity-error fallback path.
#[test]
fn multikey_stubs() {
    let mut s = KeyspaceStore::new();
    for v in [
        &b"MSET"[..] as &[u8],
        b"MGET",
        b"SINTER",
        b"SUNION",
        b"SDIFF",
        b"KEYS",
        b"SCAN",
        b"RANDOMKEY",
        b"SUBSCRIBE",
        b"PUBLISH",
    ] {
        assert_starts(&run(&mut s, &[v]), b"-ERR", "stub arity");
    }
}
