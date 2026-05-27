use super::*;

/// Dispatch a command (given as argv pieces) against `store`, returning RESP.
fn d(store: &mut Store, parts: &[&[u8]]) -> Vec<u8> {
    let args = Argv::from(parts.iter().map(|p| p.to_vec()).collect::<Vec<_>>());
    dispatch(store, &args)
}

#[test]
fn ping_and_echo() {
    let mut s = Store::new();
    assert_eq!(d(&mut s, &[b"PING"]), b"+PONG\r\n");
    assert_eq!(d(&mut s, &[b"ping", b"hi"]), b"$2\r\nhi\r\n"); // case-insensitive
    assert_eq!(d(&mut s, &[b"ECHO", b"yo"]), b"$2\r\nyo\r\n");
    assert!(d(&mut s, &[b"ECHO"]).starts_with(b"-ERR"));
}

#[test]
fn set_options_and_errors() {
    let mut s = Store::new();
    assert_eq!(d(&mut s, &[b"SET", b"k", b"v", b"NX"]), b"+OK\r\n");
    assert_eq!(d(&mut s, &[b"SET", b"k", b"w", b"NX"]), b"$-1\r\n"); // NX vetoed -> nil
    assert_eq!(d(&mut s, &[b"GET", b"k"]), b"$1\r\nv\r\n");
    assert!(d(&mut s, &[b"SET", b"k", b"v", b"EX", b"0"]).starts_with(b"-ERR")); // bad expire
    assert!(d(&mut s, &[b"SET", b"k", b"v", b"NX", b"XX"]).starts_with(b"-ERR")); // contradiction
    assert!(d(&mut s, &[b"SET", b"k"]).starts_with(b"-ERR")); // arity
}

#[test]
fn incr_paths_and_errors() {
    let mut s = Store::new();
    assert_eq!(d(&mut s, &[b"INCR", b"n"]), b":1\r\n");
    assert_eq!(d(&mut s, &[b"INCRBY", b"n", b"9"]), b":10\r\n");
    d(&mut s, &[b"SET", b"x", b"abc"]);
    assert_eq!(
        d(&mut s, &[b"INCR", b"x"]),
        b"-ERR value is not an integer or out of range\r\n"
    );
    assert!(d(&mut s, &[b"INCRBY", b"n", b"notnum"]).starts_with(b"-ERR"));
}

#[test]
fn unknown_and_config_and_type() {
    let mut s = Store::new();
    assert!(d(&mut s, &[b"FROBNICATE"]).starts_with(b"-ERR unknown command"));
    // CONFIG GET maxmemory — real handler in ops::cmd_config_get reads the
    // process Config (default maxmemory = 0). Reply is [key, value] array
    // per Redis convention.
    let reply = d(&mut s, &[b"CONFIG", b"GET", b"maxmemory"]);
    let s_reply = std::str::from_utf8(&reply).unwrap();
    assert!(s_reply.starts_with("*2\r\n"), "got {s_reply:?}");
    assert!(s_reply.contains("maxmemory"));
    // CONFIG SET is read-only in v1.0; Wave 2 wires up actual mutability.
    assert!(d(&mut s, &[b"CONFIG", b"SET", b"x", b"y"]).starts_with(b"-ERR"));
    assert_eq!(d(&mut s, &[b"TYPE", b"missing"]), b"+none\r\n");
    d(&mut s, &[b"SET", b"k", b"v"]);
    assert_eq!(d(&mut s, &[b"TYPE", b"k"]), b"+string\r\n");
    assert_eq!(d(&mut s, &[b"DBSIZE"]), b":1\r\n");
}

#[test]
fn string_completion() {
    let mut s = Store::new();
    assert_eq!(d(&mut s, &[b"SETNX", b"k", b"v1"]), b":1\r\n");
    assert_eq!(d(&mut s, &[b"SETNX", b"k", b"v2"]), b":0\r\n");
    assert_eq!(d(&mut s, &[b"GETSET", b"k", b"v3"]), b"$2\r\nv1\r\n");
    assert_eq!(d(&mut s, &[b"GETDEL", b"k"]), b"$2\r\nv3\r\n");
    assert_eq!(d(&mut s, &[b"GET", b"k"]), b"$-1\r\n");
    assert_eq!(d(&mut s, &[b"SETEX", b"t", b"100", b"x"]), b"+OK\r\n");
    assert_eq!(d(&mut s, &[b"TTL", b"t"]), b":100\r\n");
    assert_eq!(d(&mut s, &[b"INCRBYFLOAT", b"f", b"3"]), b"$1\r\n3\r\n");
    assert_eq!(d(&mut s, &[b"INCRBYFLOAT", b"f", b"1.5"]), b"$3\r\n4.5\r\n");
}

#[test]
fn routing_and_write_classification() {
    let c = KevyCommands;
    let a = |parts: &[&[u8]]| Argv::from(parts.iter().map(|p| p.to_vec()).collect::<Vec<_>>());
    assert!(matches!(c.route(&a(&[b"GET", b"k"])), Route::Single(1)));
    assert!(matches!(c.route(&a(&[b"PING"])), Route::Local));
    assert!(matches!(c.route(&a(&[b"DBSIZE"])), Route::Dbsize));
    assert!(matches!(c.route(&a(&[b"FLUSHALL"])), Route::Flush));
    assert!(matches!(c.route(&a(&[b"SAVE"])), Route::Save));
    // DEL: single key fast path vs multi-key fan-out.
    assert!(matches!(c.route(&a(&[b"DEL", b"a"])), Route::Single(1)));
    assert!(matches!(c.route(&a(&[b"DEL", b"a", b"b"])), Route::DelKeys));
    assert!(c.is_write(&a(&[b"set", b"k", b"v"])));
    assert!(!c.is_write(&a(&[b"GET", b"k"])));
    assert!(c.is_quit(&a(&[b"quit"])));
}

#[test]
fn dispatch_returns_oom_when_no_eviction_at_limit() {
    use kevy_store::EvictionPolicy;
    let mut s = Store::new();
    s.set_max_memory(1, EvictionPolicy::NoEviction);
    // First SET succeeds (precheck sees used_memory=0 ≤ maxmemory check
    // path); pushes us over.
    let reply = d(&mut s, &[b"SET", b"k", b"v"]);
    assert_eq!(reply, b"+OK\r\n");
    assert!(s.used_memory() > 1);
    // Second SET: precheck refuses with classic Redis OOM string.
    let reply = d(&mut s, &[b"SET", b"k2", b"x"]);
    let txt = std::str::from_utf8(&reply).unwrap();
    assert!(txt.starts_with("-OOM "), "expected OOM error, got {txt:?}");
    assert!(txt.contains("maxmemory"), "expected maxmemory in reply, got {txt:?}");
}

#[test]
fn dispatch_evicts_under_allkeys_random() {
    use kevy_store::EvictionPolicy;
    let mut s = Store::new();
    s.set_max_memory(800, EvictionPolicy::AllKeysRandom);
    for i in 0..30 {
        let k = format!("k{i:02}");
        d(&mut s, &[b"SET", k.as_bytes(), b"x"]);
    }
    assert!(s.used_memory() <= 800, "dispatch should keep us under: {}", s.used_memory());
    assert!(s.evictions_total() > 0, "AllKeysRandom should have evicted some keys");
}

#[test]
fn memory_usage_via_dispatch() {
    let mut s = Store::new();
    d(&mut s, &[b"SET", b"k", b"hello"]);
    let reply = d(&mut s, &[b"MEMORY", b"USAGE", b"k"]);
    // Reply is `:N\r\n` — some integer > 0.
    let txt = std::str::from_utf8(&reply).unwrap();
    assert!(txt.starts_with(":"), "expected integer reply, got {txt:?}");
    let n: i64 = txt[1..txt.len() - 2].parse().unwrap();
    assert!(n > 0);
    // Missing key → nil.
    let reply = d(&mut s, &[b"MEMORY", b"USAGE", b"missing"]);
    assert_eq!(reply, b"$-1\r\n");
}
