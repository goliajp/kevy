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
fn select_zero_ok_others_rejected() {
    let mut s = Store::new();
    // SELECT 0 — the only DB kevy serves — is acknowledged for drop-in
    // compatibility with redis-py / Jedis / etc. defaults (`db=0`).
    assert_eq!(d(&mut s, &[b"SELECT", b"0"]), b"+OK\r\n");
    assert_eq!(d(&mut s, &[b"select", b"0"]), b"+OK\r\n");

    // Any other index returns an explicit kevy-only-DB-0 error (NOT the
    // valkey "DB index is out of range" — that one would mislead callers
    // into thinking they could just config their way around it).
    let r1 = d(&mut s, &[b"SELECT", b"1"]);
    assert!(r1.starts_with(b"-ERR kevy only supports DB 0"), "got: {:?}", std::str::from_utf8(&r1));
    let r15 = d(&mut s, &[b"SELECT", b"15"]);
    assert!(r15.starts_with(b"-ERR kevy only supports DB 0"));

    // Non-numeric and out-of-range get Redis's "value is not an integer".
    assert_eq!(
        d(&mut s, &[b"SELECT", b"abc"]),
        b"-ERR value is not an integer or out of range\r\n"
    );

    // Arity error matches the existing convention.
    assert!(d(&mut s, &[b"SELECT"]).starts_with(b"-ERR"));
    assert!(d(&mut s, &[b"SELECT", b"0", b"extra"]).starts_with(b"-ERR"));
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

// ============================================================
// KevyCommands trait matrix: `route` / `resolve` / `is_write` / `txn_kind` /
// `is_quit`. The reactor's hot path uses `resolve` exclusively; the standalone
// `route` / `is_write` / `txn_kind` exist as a fallback API and a small
// readability win, but get little integration coverage. These tests pin every
// arm so the verb tables can't drift silently.
// ============================================================

fn argv(parts: &[&[u8]]) -> Argv {
    Argv::from(parts.iter().map(|p| p.to_vec()).collect::<Vec<_>>())
}

#[test]
fn route_local_verbs() {
    let c = KevyCommands;
    for v in [
        "PING", "ECHO", "QUIT", "COMMAND", "CONFIG", "INFO", "CLUSTER",
        "DEBUG", "WAIT", "SHUTDOWN", "CLIENT",
    ] {
        assert!(
            matches!(c.route(&argv(&[v.as_bytes()])), Route::Local),
            "{v}"
        );
    }
    // HELLO is now Route::Hello (its own conn-level handler — v1.4.0
    // gained HELLO 3 + per-conn RespVersion negotiation).
    assert!(matches!(c.route(&argv(&[b"HELLO"])), Route::Hello));
    assert!(matches!(c.route(&argv(&[b"HELLO", b"3"])), Route::Hello));
    // Empty args is also Route::Local (server-side error reply).
    assert!(matches!(c.route(&argv(&[])), Route::Local));
}

#[test]
fn route_keyspace_verbs() {
    let c = KevyCommands;
    assert!(matches!(c.route(&argv(&[b"DBSIZE"])), Route::Dbsize));
    assert!(matches!(c.route(&argv(&[b"FLUSHDB"])), Route::Flush));
    assert!(matches!(c.route(&argv(&[b"FLUSHALL"])), Route::Flush));
    assert!(matches!(c.route(&argv(&[b"SAVE"])), Route::Save));
    assert!(matches!(c.route(&argv(&[b"BGSAVE"])), Route::Save));
    assert!(matches!(c.route(&argv(&[b"BGREWRITEAOF"])), Route::RewriteAof));
    assert!(matches!(c.route(&argv(&[b"RANDOMKEY"])), Route::RandomKey));
}

#[test]
fn route_multikey_and_pubsub_verbs() {
    let c = KevyCommands;
    // MSET takes pairs: total argv length must be odd (verb + 2N).
    assert!(matches!(
        c.route(&argv(&[b"MSET", b"k1", b"v1", b"k2", b"v2"])),
        Route::MSet
    ));
    assert!(matches!(
        c.route(&argv(&[b"MGET", b"k1", b"k2"])),
        Route::MGet
    ));
    assert!(matches!(
        c.route(&argv(&[b"SINTER", b"s1", b"s2"])),
        Route::SInter
    ));
    assert!(matches!(
        c.route(&argv(&[b"SUNION", b"s1", b"s2"])),
        Route::SUnion
    ));
    assert!(matches!(
        c.route(&argv(&[b"SDIFF", b"s1", b"s2"])),
        Route::SDiff
    ));
    assert!(matches!(c.route(&argv(&[b"KEYS", b"*"])), Route::Keys(_)));
    assert!(matches!(c.route(&argv(&[b"SCAN", b"0"])), Route::Scan(_)));
    assert!(matches!(
        c.route(&argv(&[b"SUBSCRIBE", b"chan"])),
        Route::Subscribe
    ));
    assert!(matches!(c.route(&argv(&[b"UNSUBSCRIBE"])), Route::Unsubscribe));
    assert!(matches!(
        c.route(&argv(&[b"PUBLISH", b"chan", b"msg"])),
        Route::Publish
    ));
}

#[test]
fn route_del_exists_arity_branches() {
    let c = KevyCommands;
    // Single-key DEL / EXISTS hit the fast path Route::Single(1).
    assert!(matches!(c.route(&argv(&[b"DEL", b"k"])), Route::Single(1)));
    assert!(matches!(c.route(&argv(&[b"EXISTS", b"k"])), Route::Single(1)));
    // Multi-key DEL / EXISTS switch to dedicated fanout routes.
    assert!(matches!(
        c.route(&argv(&[b"DEL", b"k1", b"k2"])),
        Route::DelKeys
    ));
    assert!(matches!(
        c.route(&argv(&[b"EXISTS", b"k1", b"k2"])),
        Route::ExistsKeys
    ));
    // Generic single-key default path.
    assert!(matches!(c.route(&argv(&[b"GET", b"k"])), Route::Single(1)));
    // Unknown verb with no key → Local (so dispatch can return the error).
    assert!(matches!(c.route(&argv(&[b"UNKNOWNCMD"])), Route::Local));
}

#[test]
fn is_write_and_txn_kind_matrix() {
    let c = KevyCommands;
    // Write verbs.
    for v in ["SET", "DEL", "HSET", "LPUSH", "SADD", "ZADD", "INCR"] {
        assert!(c.is_write(&argv(&[v.as_bytes()])), "{v} should be write");
    }
    // Read verbs.
    for v in ["GET", "HGET", "LLEN", "TYPE"] {
        assert!(!c.is_write(&argv(&[v.as_bytes()])), "{v} should be read");
    }
    // Empty args → not a write.
    assert!(!c.is_write(&argv(&[])));

    // Transaction control verbs.
    assert!(matches!(c.txn_kind(&argv(&[b"MULTI"])), TxnKind::Multi));
    assert!(matches!(c.txn_kind(&argv(&[b"EXEC"])), TxnKind::Exec));
    assert!(matches!(c.txn_kind(&argv(&[b"DISCARD"])), TxnKind::Discard));
    assert!(matches!(c.txn_kind(&argv(&[b"SET", b"k", b"v"])), TxnKind::Other));
    assert!(matches!(c.txn_kind(&argv(&[])), TxnKind::Other));

    // is_quit covers QUIT case-insensitively.
    assert!(c.is_quit(&argv(&[b"QUIT"])));
    assert!(c.is_quit(&argv(&[b"quit"])));
    assert!(!c.is_quit(&argv(&[b"PING"])));
    assert!(!c.is_quit(&argv(&[])));
}

#[test]
fn resolve_unifies_route_and_txn_kind() {
    // The hot-path `resolve` should agree with the individual accessors for
    // every verb category. Pin a representative each, plus the empty-args
    // sentinel, so the duplicated match table can't drift.
    let c = KevyCommands;
    let cases: &[(&[&[u8]], TxnKind, bool, bool)] = &[
        (&[b"PING"], TxnKind::Other, false, false),
        (&[b"QUIT"], TxnKind::Other, true, false),
        (&[b"MULTI"], TxnKind::Multi, false, false),
        (&[b"EXEC"], TxnKind::Exec, false, false),
        (&[b"DISCARD"], TxnKind::Discard, false, false),
        (&[b"SET", b"k", b"v"], TxnKind::Other, false, true),
        (&[b"GET", b"k"], TxnKind::Other, false, false),
        (&[b"DEL", b"k1", b"k2"], TxnKind::Other, false, true),
        (&[], TxnKind::Other, false, false),
    ];
    for (parts, kind, quit, write) in cases {
        let r = c.resolve(&argv(parts));
        assert_eq!(
            std::mem::discriminant(&r.txn_kind),
            std::mem::discriminant(kind),
            "{parts:?}: txn_kind discriminant mismatch"
        );
        assert_eq!(r.is_quit, *quit, "{parts:?} is_quit");
        assert_eq!(r.is_write, *write, "{parts:?} is_write");
    }
    // Also pin every route arm via resolve, so resolve's match table stays
    // in sync with the standalone `route`. Pick distinct-route verbs:
    for (parts, want) in [
        (vec![b"PING".as_ref()], Route::Local),
        (vec![b"DBSIZE".as_ref()], Route::Dbsize),
        (vec![b"FLUSHDB".as_ref()], Route::Flush),
        (vec![b"SAVE".as_ref()], Route::Save),
        (vec![b"BGREWRITEAOF".as_ref()], Route::RewriteAof),
        (vec![b"SUBSCRIBE".as_ref(), b"c".as_ref()], Route::Subscribe),
        (vec![b"UNSUBSCRIBE".as_ref()], Route::Unsubscribe),
        (vec![b"PUBLISH".as_ref(), b"c".as_ref(), b"m".as_ref()], Route::Publish),
        (vec![b"RANDOMKEY".as_ref()], Route::RandomKey),
        (vec![b"DEL".as_ref(), b"a".as_ref(), b"b".as_ref()], Route::DelKeys),
        (vec![b"EXISTS".as_ref(), b"a".as_ref(), b"b".as_ref()], Route::ExistsKeys),
    ] {
        let argv = argv(&parts);
        let resolved = c.resolve(&argv);
        let routed = c.route(&argv);
        // Compare by discriminant (Route doesn't impl Debug; payload-bearing
        // arms like Keys / Scan / Single carry data so we only check kind).
        let want_d = std::mem::discriminant(&want);
        assert_eq!(
            std::mem::discriminant(&resolved.route),
            want_d,
            "{parts:?}: resolve produced wrong route variant"
        );
        assert_eq!(
            std::mem::discriminant(&routed),
            want_d,
            "{parts:?}: route produced wrong route variant"
        );
    }
}

#[test]
fn drain_commands_handles_quit_and_protocol_error() {
    let mut store = Store::new();

    // Normal command + QUIT returns Close, with both replies appended.
    let mut input = b"*1\r\n$4\r\nPING\r\n*1\r\n$4\r\nQUIT\r\n".to_vec();
    let mut output = Vec::new();
    let r = drain_commands(&mut store, &mut input, &mut output);
    assert!(
        matches!(r, AfterDrain::Close),
        "expected Close after QUIT — input remaining: {:?}, output: {:?}",
        std::str::from_utf8(&input),
        std::str::from_utf8(&output),
    );
    assert!(output.starts_with(b"+PONG\r\n"));
    assert!(output.ends_with(b"+OK\r\n"));

    // Incomplete frame → KeepOpen (no reply, partial bytes left in input).
    let mut input = b"*1\r\n$4\r\nPIN".to_vec(); // truncated mid-bulk
    let mut output = Vec::new();
    let r = drain_commands(&mut store, &mut input, &mut output);
    assert!(matches!(r, AfterDrain::KeepOpen));
    assert!(output.is_empty());

    // Malformed protocol → Close + "-ERR Protocol error" reply.
    // *Z is a non-numeric array length — parser rejects with Err.
    let mut input = b"*Z\r\n".to_vec();
    let mut output = Vec::new();
    let r = drain_commands(&mut store, &mut input, &mut output);
    assert!(
        matches!(r, AfterDrain::Close),
        "expected Close on bad RESP — output: {:?}",
        std::str::from_utf8(&output)
    );
    let s = std::str::from_utf8(&output).unwrap();
    assert!(s.contains("Protocol error"), "got: {s:?}");
}

#[test]
fn config_enum_mapping_round_trips() {
    // Cover map_eviction_policy + map_appendfsync — pure data maps. If a
    // policy lands in one enum but the other forgets the case, this fails.
    use kevy_config::AppendFsync as CA;
    use kevy_config::EvictionPolicy as CE;
    use kevy_persist::Fsync as P;
    use kevy_store::EvictionPolicy as S;

    let evict_cases = [
        (CE::NoEviction, S::NoEviction),
        (CE::AllKeysLru, S::AllKeysLru),
        (CE::AllKeysLfu, S::AllKeysLfu),
        (CE::AllKeysRandom, S::AllKeysRandom),
        (CE::VolatileLru, S::VolatileLru),
        (CE::VolatileLfu, S::VolatileLfu),
        (CE::VolatileRandom, S::VolatileRandom),
        (CE::VolatileTtl, S::VolatileTtl),
    ];
    for (src, dst) in evict_cases {
        assert_eq!(map_eviction_policy(src), dst);
    }

    let fsync_cases = [
        (CA::Always, P::Always),
        (CA::EverySec, P::EverySec),
        (CA::No, P::No),
    ];
    for (src, dst) in fsync_cases {
        assert_eq!(
            std::mem::discriminant(&map_appendfsync(src)),
            std::mem::discriminant(&dst)
        );
    }
}

#[test]
fn shard_tick_interval_falls_back_to_disabled() {
    // With no `config_init` called (the test process default), the global
    // config returns `Config::default()`. With default hz != 0 we get a
    // non-zero interval; we mainly assert the function is callable.
    let c = KevyCommands;
    let ms = c.shard_tick_interval_ms();
    // Either disabled (hz=0) or capped 1..=10_000.
    assert!(ms == 0 || (1..=10_000).contains(&ms), "got {ms}");
}
