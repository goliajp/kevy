//! End-to-end EVAL / EVALSHA / SCRIPT through the kevy command
//! dispatch path. Verifies v1.27 P7b — `cmd_lua` is wired into
//! `dispatch.rs` and the `LuaHost` correctly routes redis.call
//! through `kevy::dispatch_into` against the real `Store`.

use kevy_resp::Argv;
use kevy_store::Store;

/// Build an Argv from a slice of byte slices. Helper for the
/// EVAL <script> <numkeys> <key>... <arg>... protocol shape.
fn argv(parts: &[&[u8]]) -> Argv {
    let mut a = Argv::default();
    for p in parts {
        a.push(p);
    }
    a
}

#[test]
fn eval_pure_lua_no_redis_call() {
    let mut store = Store::new();
    let reply = kevy::dispatch(&mut store, &argv(&[b"EVAL", b"return 1", b"0"]));
    assert_eq!(reply, b":1\r\n");
}

#[test]
fn eval_redis_call_set_then_get_round_trips() {
    let mut store = Store::new();
    let script = b"redis.call('SET', KEYS[1], ARGV[1])\n\
                   return redis.call('GET', KEYS[1])\n";
    let reply = kevy::dispatch(
        &mut store,
        &argv(&[b"EVAL", script, b"1", b"mykey", b"hello"]),
    );
    assert_eq!(reply, b"$5\r\nhello\r\n");
    // Confirm the kevy Store actually got the SET.
    let reply2 = kevy::dispatch(&mut store, &argv(&[b"GET", b"mykey"]));
    assert_eq!(reply2, b"$5\r\nhello\r\n");
}

#[test]
fn eval_uses_kevy_incr_through_redis_call() {
    let mut store = Store::new();
    let reply = kevy::dispatch(
        &mut store,
        &argv(&[
            b"EVAL",
            b"redis.call('INCR', KEYS[1])\n\
              redis.call('INCRBY', KEYS[1], 10)\n\
              return redis.call('GET', KEYS[1])\n",
            b"1",
            b"counter",
        ]),
    );
    assert_eq!(reply, b"$2\r\n11\r\n");
}

#[test]
fn eval_with_wrong_numkeys_returns_resp_error() {
    let mut store = Store::new();
    let reply = kevy::dispatch(
        &mut store,
        &argv(&[b"EVAL", b"return 1", b"5", b"only-one-key"]),
    );
    assert!(reply.starts_with(b"-ERR "));
}

#[test]
fn eval_missing_args_returns_wrong_args_err() {
    let mut store = Store::new();
    let reply = kevy::dispatch(&mut store, &argv(&[b"EVAL"]));
    assert!(reply.starts_with(b"-ERR "));
}

#[test]
fn script_load_then_evalsha_round_trips() {
    let mut store = Store::new();
    let load_reply = kevy::dispatch(
        &mut store,
        &argv(&[b"SCRIPT", b"LOAD", b"return 'cached'"]),
    );
    // Reply is a bulk string of 40 hex chars.
    assert!(load_reply.starts_with(b"$40\r\n"));
    let sha_hex = &load_reply[5..45]; // skip "$40\r\n" prefix
    let evalsha_argv = vec![&b"EVALSHA"[..], sha_hex, &b"0"[..]];
    let evalsha_reply = kevy::dispatch(&mut store, &argv(&evalsha_argv));
    assert_eq!(evalsha_reply, b"$6\r\ncached\r\n");
    let _ = evalsha_argv;
}

#[test]
fn script_exists_reports_hits_and_misses() {
    let mut store = Store::new();
    let load_reply = kevy::dispatch(
        &mut store,
        &argv(&[b"SCRIPT", b"LOAD", b"return 42"]),
    );
    let sha_hex = load_reply[5..45].to_vec();
    let missing_sha = b"0".repeat(40);
    let reply = kevy::dispatch(
        &mut store,
        &argv(&[b"SCRIPT", b"EXISTS", &sha_hex, &missing_sha]),
    );
    assert_eq!(reply, b"*2\r\n:1\r\n:0\r\n");
}

#[test]
fn script_flush_clears_cache() {
    let mut store = Store::new();
    let load_reply = kevy::dispatch(
        &mut store,
        &argv(&[b"SCRIPT", b"LOAD", b"return 42"]),
    );
    let sha_hex = load_reply[5..45].to_vec();
    let flush_reply = kevy::dispatch(&mut store, &argv(&[b"SCRIPT", b"FLUSH"]));
    assert_eq!(flush_reply, b"+OK\r\n");
    let exists = kevy::dispatch(
        &mut store,
        &argv(&[b"SCRIPT", b"EXISTS", &sha_hex]),
    );
    assert_eq!(exists, b"*1\r\n:0\r\n");
    // Cached script no longer reachable.
    let evalsha_argv = vec![&b"EVALSHA"[..], &sha_hex[..], &b"0"[..]];
    let evalsha_reply = kevy::dispatch(&mut store, &argv(&evalsha_argv));
    assert!(evalsha_reply.starts_with(b"-NOSCRIPT "));
    let _ = evalsha_argv;
}

#[test]
fn eval_redlock_unlock_canonical_script() {
    let mut store = Store::new();
    // Pre-seed the lock with the expected token.
    let _ = kevy::dispatch(
        &mut store,
        &argv(&[b"SET", b"lock:foo", b"token-abc"]),
    );
    // The byte-for-byte canonical Redlock unlock script.
    let script = b"if redis.call('GET', KEYS[1]) == ARGV[1] then\n\
                       return redis.call('DEL', KEYS[1])\n\
                   else\n\
                       return 0\n\
                   end\n";
    let reply = kevy::dispatch(
        &mut store,
        &argv(&[b"EVAL", script, b"1", b"lock:foo", b"token-abc"]),
    );
    assert_eq!(reply, b":1\r\n");
    // Lock is gone.
    let get_reply = kevy::dispatch(&mut store, &argv(&[b"GET", b"lock:foo"]));
    assert_eq!(get_reply, b"$-1\r\n");
}

#[test]
fn eval_redlock_unlock_token_mismatch_returns_zero() {
    let mut store = Store::new();
    let _ = kevy::dispatch(
        &mut store,
        &argv(&[b"SET", b"lock:foo", b"someone-else"]),
    );
    let script = b"if redis.call('GET', KEYS[1]) == ARGV[1] then\n\
                       return redis.call('DEL', KEYS[1])\n\
                   else\n\
                       return 0\n\
                   end\n";
    let reply = kevy::dispatch(
        &mut store,
        &argv(&[b"EVAL", script, b"1", b"lock:foo", b"my-token"]),
    );
    assert_eq!(reply, b":0\r\n");
    let get_reply = kevy::dispatch(&mut store, &argv(&[b"GET", b"lock:foo"]));
    assert_eq!(get_reply, b"$12\r\nsomeone-else\r\n");
}

#[test]
fn eval_shebang_lua_53_integer_divide_through_kevy() {
    let mut store = Store::new();
    let reply = kevy::dispatch(
        &mut store,
        &argv(&[
            b"EVAL",
            b"#!lua version=5.3\nreturn redis.call('INCRBY', KEYS[1], 10 // 3)",
            b"1",
            b"counter",
        ]),
    );
    // 10 // 3 = 3 (5.3+ integer divide) → INCRBY counter 3 → :3
    assert_eq!(reply, b":3\r\n");
}

// ─────────────────────────────────────────────────────────────────────
// P7c — EVAL_RO / EVALSHA_RO write-flag enforcement
// ─────────────────────────────────────────────────────────────────────

#[test]
fn eval_ro_blocks_set() {
    let mut store = Store::new();
    let reply = kevy::dispatch(
        &mut store,
        &argv(&[b"EVAL_RO", b"return redis.call('SET', KEYS[1], 'v')", b"1", b"k"]),
    );
    assert!(reply.starts_with(b"-READONLY "));
}

#[test]
fn eval_ro_allows_get() {
    let mut store = Store::new();
    let _ = kevy::dispatch(&mut store, &argv(&[b"SET", b"k", b"hello"]));
    let reply = kevy::dispatch(
        &mut store,
        &argv(&[b"EVAL_RO", b"return redis.call('GET', KEYS[1])", b"1", b"k"]),
    );
    assert_eq!(reply, b"$5\r\nhello\r\n");
}

#[test]
fn eval_ro_blocks_del() {
    let mut store = Store::new();
    let _ = kevy::dispatch(&mut store, &argv(&[b"SET", b"k", b"v"]));
    let reply = kevy::dispatch(
        &mut store,
        &argv(&[b"EVAL_RO", b"return redis.call('DEL', KEYS[1])", b"1", b"k"]),
    );
    assert!(reply.starts_with(b"-READONLY "));
    // Key still present.
    assert_eq!(
        kevy::dispatch(&mut store, &argv(&[b"EXISTS", b"k"])),
        b":1\r\n"
    );
}

#[test]
fn eval_ro_blocks_incrby_via_pcall_returns_err_table() {
    let mut store = Store::new();
    let reply = kevy::dispatch(
        &mut store,
        &argv(&[
            b"EVAL_RO",
            b"return redis.pcall('INCRBY', KEYS[1], 5)",
            b"1",
            b"counter",
        ]),
    );
    // pcall catches the error → {err = "READONLY ..."} → -READONLY ...
    assert!(reply.starts_with(b"-READONLY "));
}

#[test]
fn evalsha_ro_blocks_write_in_cached_script() {
    let mut store = Store::new();
    let load_reply = kevy::dispatch(
        &mut store,
        &argv(&[b"SCRIPT", b"LOAD", b"return redis.call('SET', KEYS[1], ARGV[1])"]),
    );
    let sha_hex = load_reply[5..45].to_vec();
    let ro = kevy::dispatch(
        &mut store,
        &argv(&[b"EVALSHA_RO", &sha_hex, b"1", b"k", b"v"]),
    );
    assert!(ro.starts_with(b"-READONLY "));
    // Same SHA via writeable EVALSHA works.
    let rw = kevy::dispatch(
        &mut store,
        &argv(&[b"EVALSHA", &sha_hex, b"1", b"k", b"v"]),
    );
    assert_eq!(rw, b"+OK\r\n");
}

#[test]
fn eval_writeable_resumes_after_eval_ro() {
    let mut store = Store::new();
    let r1 = kevy::dispatch(
        &mut store,
        &argv(&[b"EVAL_RO", b"return redis.call('SET', KEYS[1], 'v')", b"1", b"k"]),
    );
    assert!(r1.starts_with(b"-READONLY "));
    // The next non-RO EVAL writes fine — the read_only flag was
    // cleared by the LuaHost::eval_ro RAII guard.
    let r2 = kevy::dispatch(
        &mut store,
        &argv(&[b"EVAL", b"return redis.call('SET', KEYS[1], 'v')", b"1", b"k"]),
    );
    assert_eq!(r2, b"+OK\r\n");
}
