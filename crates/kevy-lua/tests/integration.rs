//! Integration tests for the public `kevy-lua` surface — anything
//! that only needs `Bridge::with_no_dispatch()` / `Bridge::eval()` / `FlushMode`
//! lives here so the `src/lib.rs` body stays well under the 500-LOC
//! house-rule ceiling. Tests that touch the per-Vm pool size
//! (`Bridge::vm_count`, `#[cfg(test)]`-gated) stay in `src/lib.rs`.

use kevy_lua::{Bridge, FlushMode};

// ─────────────────────────────────────────────────────────────────────
// P1 — return values from pure Lua
// ─────────────────────────────────────────────────────────────────────

#[test]
fn eval_return_one_is_resp_integer_one() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.eval(b"return 1", &[], &[]);
    assert_eq!(reply, b":1\r\n", "got: {:?}", String::from_utf8_lossy(&reply));
}

#[test]
fn eval_return_string_is_resp_bulk_string() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.eval(b"return 'hello'", &[], &[]);
    assert_eq!(reply, b"$5\r\nhello\r\n");
}

#[test]
fn eval_return_nil_is_resp_nil_bulk() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.eval(b"return nil", &[], &[]);
    assert_eq!(reply, b"$-1\r\n");
}

#[test]
fn eval_return_true_is_resp_integer_one() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.eval(b"return true", &[], &[]);
    assert_eq!(reply, b":1\r\n");
}

#[test]
fn eval_return_false_is_resp_nil_bulk() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.eval(b"return false", &[], &[]);
    assert_eq!(reply, b"$-1\r\n");
}

#[test]
fn eval_syntax_error_is_resp_error() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.eval(b"return ((", &[], &[]);
    assert!(reply.starts_with(b"-ERR "));
    assert!(reply.ends_with(b"\r\n"));
}

#[test]
fn eval_no_return_is_resp_nil_bulk() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.eval(b"local x = 1", &[], &[]);
    assert_eq!(reply, b"$-1\r\n");
}

#[test]
fn eval_non_utf8_script_is_resp_error() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.eval(&[0xff, 0xfe], &[], &[]);
    assert!(reply.starts_with(b"-ERR "));
}

// ─────────────────────────────────────────────────────────────────────
// P2 — table marshaling
// ─────────────────────────────────────────────────────────────────────

#[test]
fn eval_ok_table_is_simple_string() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.eval(b"return {ok = 'OK'}", &[], &[]);
    assert_eq!(reply, b"+OK\r\n");
}

#[test]
fn eval_err_table_is_resp_error() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.eval(b"return {err = 'something broke'}", &[], &[]);
    assert_eq!(reply, b"-ERR something broke\r\n");
}

#[test]
fn eval_err_table_with_kind_passes_through() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.eval(b"return {err = 'NOSCRIPT no script'}", &[], &[]);
    assert_eq!(reply, b"-NOSCRIPT no script\r\n");
}

#[test]
fn eval_array_table_is_resp_array() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.eval(b"return {1, 2, 3}", &[], &[]);
    assert_eq!(reply, b"*3\r\n:1\r\n:2\r\n:3\r\n");
}

#[test]
fn eval_array_table_stops_at_first_nil() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.eval(b"return {1, nil, 3}", &[], &[]);
    assert_eq!(reply, b"*1\r\n:1\r\n");
}

#[test]
fn eval_empty_table_is_empty_array() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.eval(b"return {}", &[], &[]);
    assert_eq!(reply, b"*0\r\n");
}

#[test]
fn eval_mixed_type_array() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.eval(b"return {1, 'hello', true}", &[], &[]);
    assert_eq!(reply, b"*3\r\n:1\r\n$5\r\nhello\r\n:1\r\n");
}

#[test]
fn eval_nested_array() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.eval(b"return {1, {2, 3}}", &[], &[]);
    assert_eq!(reply, b"*2\r\n:1\r\n*2\r\n:2\r\n:3\r\n");
}

#[test]
fn eval_err_beats_ok_when_both_present() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.eval(b"return {ok = 'OK', err = 'oops'}", &[], &[]);
    assert_eq!(reply, b"-ERR oops\r\n");
}

#[test]
fn eval_float_non_integral_is_bulk() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.eval(b"return 1.5", &[], &[]);
    assert_eq!(reply, b"$3\r\n1.5\r\n");
}

#[test]
fn eval_binary_safe_string() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.eval(b"return '\\0\\1\\255'", &[], &[]);
    assert_eq!(reply, b"$3\r\n\x00\x01\xff\r\n");
}

// ─────────────────────────────────────────────────────────────────────
// P3a — KEYS / ARGV globals + redis host table presence
// ─────────────────────────────────────────────────────────────────────

#[test]
fn eval_keys_first_element_reflects_argv() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.eval(b"return KEYS[1]", &[b"mykey"], &[]);
    assert_eq!(reply, b"$5\r\nmykey\r\n");
}

#[test]
fn eval_argv_first_element_reflects_args() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.eval(b"return ARGV[1]", &[], &[b"myval"]);
    assert_eq!(reply, b"$5\r\nmyval\r\n");
}

#[test]
fn eval_keys_length() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.eval(b"return #KEYS", &[b"k1", b"k2", b"k3"], &[]);
    assert_eq!(reply, b":3\r\n");
}

#[test]
fn eval_argv_length() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.eval(b"return #ARGV", &[], &[b"a1", b"a2"]);
    assert_eq!(reply, b":2\r\n");
}

#[test]
fn eval_empty_keys_and_args() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.eval(b"return #KEYS + #ARGV", &[], &[]);
    assert_eq!(reply, b":0\r\n");
}

#[test]
fn eval_binary_safe_keys_argv() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.eval(b"return KEYS[1]", &[b"\x00\x01\xff"], &[]);
    assert_eq!(reply, b"$3\r\n\x00\x01\xff\r\n");
}

#[test]
fn eval_keys_argv_rebind_between_calls() {
    let mut b = Bridge::with_no_dispatch();
    let r1 = b.eval(b"return KEYS[1]", &[b"first"], &[]);
    let r2 = b.eval(b"return KEYS[1]", &[b"second"], &[]);
    assert_eq!(r1, b"$5\r\nfirst\r\n");
    assert_eq!(r2, b"$6\r\nsecond\r\n");
}

#[test]
fn eval_redis_is_a_table() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.eval(b"return type(redis)", &[], &[]);
    assert_eq!(reply, b"$5\r\ntable\r\n");
}

#[test]
fn eval_redis_call_is_a_function() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.eval(b"return type(redis.call)", &[], &[]);
    assert_eq!(reply, b"$8\r\nfunction\r\n");
}

#[test]
fn eval_redis_method_surface_all_present() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.eval(
        b"return type(redis.call) == 'function'\
          and type(redis.pcall) == 'function'\
          and type(redis.status_reply) == 'function'\
          and type(redis.error_reply) == 'function'\
          and type(redis.sha1hex) == 'function'\
          and type(redis.log) == 'function'\
          and type(redis.replicate_commands) == 'function'",
        &[],
        &[],
    );
    assert_eq!(reply, b":1\r\n");
}

#[test]
fn eval_redis_status_reply_round_trips_as_simple_string() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.eval(b"return redis.status_reply('PONG')", &[], &[]);
    assert_eq!(reply, b"+PONG\r\n");
}

#[test]
fn eval_redis_error_reply_round_trips_as_error() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.eval(b"return redis.error_reply('NOSCRIPT no script')", &[], &[]);
    assert_eq!(reply, b"-NOSCRIPT no script\r\n");
}

#[test]
fn eval_redis_sha1hex_returns_40_chars() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.eval(b"return redis.sha1hex('whatever')", &[], &[]);
    assert_eq!(
        reply,
        b"$40\r\n0000000000000000000000000000000000000000\r\n"
    );
}

#[test]
fn eval_redis_log_returns_nothing() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.eval(b"redis.log(2, 'hi') return 'after'", &[], &[]);
    assert_eq!(reply, b"$5\r\nafter\r\n");
}

#[test]
fn eval_redis_replicate_commands_returns_nothing() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.eval(b"redis.replicate_commands() return 'after'", &[], &[]);
    assert_eq!(reply, b"$5\r\nafter\r\n");
}

#[test]
fn eval_redis_call_no_dispatch_raises_lua_error() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.eval(b"return redis.call('SET', 'k', 'v')", &[], &[]);
    assert!(reply.starts_with(b"-ERR "));
    assert!(
        reply.windows(20).any(|w| w == b"no host dispatch wir"),
        "want 'no host dispatch wired' in: {:?}",
        String::from_utf8_lossy(&reply)
    );
}

#[test]
fn eval_redis_pcall_no_dispatch_returns_err_table() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.eval(b"return redis.pcall('SET', 'k', 'v')", &[], &[]);
    assert!(reply.starts_with(b"-ERR "));
    assert!(reply.windows(20).any(|w| w == b"no host dispatch wir"));
}

// ─────────────────────────────────────────────────────────────────────
// P3b — redis.call wired through a stub host dispatch (in-memory
// keyspace). End-to-end EVAL of "redis.call('SET',KEYS[1],ARGV[1]);
// return redis.call('GET',KEYS[1])" should round-trip the value.
// ─────────────────────────────────────────────────────────────────────

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// Stub host dispatcher: a tiny in-memory string keyspace handling
/// GET / SET / DEL / EXISTS / INCRBY / PING. Returns RESP reply
/// bytes per the protocol — the same shape kevy-rt's dispatcher
/// will produce in production.
fn make_stub_dispatch()
-> (Rc<RefCell<HashMap<Vec<u8>, Vec<u8>>>>, impl Fn(&[&[u8]]) -> Vec<u8> + 'static)
{
    let store: Rc<RefCell<HashMap<Vec<u8>, Vec<u8>>>> =
        Rc::new(RefCell::new(HashMap::new()));
    let store_in = Rc::clone(&store);
    let dispatch = move |argv: &[&[u8]]| -> Vec<u8> {
        if argv.is_empty() {
            return b"-ERR no command\r\n".to_vec();
        }
        let cmd: Vec<u8> = argv[0].iter().map(|b| b.to_ascii_uppercase()).collect();
        let store = &mut *store_in.borrow_mut();
        match cmd.as_slice() {
            b"PING" => b"+PONG\r\n".to_vec(),
            b"GET" if argv.len() == 2 => match store.get(argv[1]) {
                Some(v) => {
                    let mut out = format!("${}\r\n", v.len()).into_bytes();
                    out.extend_from_slice(v);
                    out.extend_from_slice(b"\r\n");
                    out
                }
                None => b"$-1\r\n".to_vec(),
            },
            b"SET" if argv.len() >= 3 => {
                store.insert(argv[1].to_vec(), argv[2].to_vec());
                b"+OK\r\n".to_vec()
            }
            b"DEL" if argv.len() >= 2 => {
                let mut n = 0i64;
                for k in &argv[1..] {
                    if store.remove(*k).is_some() {
                        n += 1;
                    }
                }
                format!(":{n}\r\n").into_bytes()
            }
            b"EXISTS" if argv.len() >= 2 => {
                let mut n = 0i64;
                for k in &argv[1..] {
                    if store.contains_key(*k) {
                        n += 1;
                    }
                }
                format!(":{n}\r\n").into_bytes()
            }
            b"INCRBY" if argv.len() == 3 => {
                let delta: i64 = std::str::from_utf8(argv[2])
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                let cur: i64 = store
                    .get(argv[1])
                    .and_then(|b| std::str::from_utf8(b).ok())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                let new = cur + delta;
                store.insert(argv[1].to_vec(), new.to_string().into_bytes());
                format!(":{new}\r\n").into_bytes()
            }
            _ => format!(
                "-ERR stub dispatcher: unknown command {:?}\r\n",
                String::from_utf8_lossy(argv[0])
            )
            .into_bytes(),
        }
    };
    (store, dispatch)
}

#[test]
fn eval_redis_call_set_then_get_round_trips() {
    let (_store, dispatch) = make_stub_dispatch();
    let mut b = Bridge::new(dispatch);
    let reply = b.eval(
        b"redis.call('SET', KEYS[1], ARGV[1])\
          return redis.call('GET', KEYS[1])",
        &[b"hot:key"],
        &[b"v1"],
    );
    assert_eq!(reply, b"$2\r\nv1\r\n");
}

#[test]
fn eval_redis_call_ping_status_reply_marshals_simple_string() {
    let (_store, dispatch) = make_stub_dispatch();
    let mut b = Bridge::new(dispatch);
    let reply = b.eval(b"return redis.call('PING')", &[], &[]);
    // redis.call returns the dispatch's `+PONG\r\n` reply parsed back
    // to Lua as {ok="PONG"}, which marshals back to RESP as +PONG\r\n.
    assert_eq!(reply, b"+PONG\r\n");
}

#[test]
fn eval_redis_call_get_missing_returns_false() {
    let (_store, dispatch) = make_stub_dispatch();
    let mut b = Bridge::new(dispatch);
    let reply = b.eval(
        b"return redis.call('GET', 'never:set')",
        &[],
        &[],
    );
    // `$-1\r\n` → Lua boolean false → marshaled back as nil bulk.
    assert_eq!(reply, b"$-1\r\n");
}

#[test]
fn eval_redis_call_exists_returns_integer() {
    let (_store, dispatch) = make_stub_dispatch();
    let mut b = Bridge::new(dispatch);
    let reply = b.eval(
        b"redis.call('SET', 'a', '1')\
          redis.call('SET', 'b', '2')\
          return redis.call('EXISTS', 'a', 'b', 'c')",
        &[],
        &[],
    );
    assert_eq!(reply, b":2\r\n");
}

#[test]
fn eval_redis_call_incrby_returns_new_value() {
    let (_store, dispatch) = make_stub_dispatch();
    let mut b = Bridge::new(dispatch);
    let reply = b.eval(
        b"redis.call('INCRBY', 'hits', 5)\
          return redis.call('INCRBY', 'hits', 3)",
        &[],
        &[],
    );
    assert_eq!(reply, b":8\r\n");
}

#[test]
fn eval_redis_call_unknown_command_raises_lua_error() {
    let (_store, dispatch) = make_stub_dispatch();
    let mut b = Bridge::new(dispatch);
    let reply = b.eval(b"return redis.call('FAKECMD')", &[], &[]);
    // -ERR from the stub becomes a Lua error → RESP error reply.
    assert!(reply.starts_with(b"-ERR "), "got: {:?}", String::from_utf8_lossy(&reply));
}

#[test]
fn eval_redis_pcall_unknown_command_returns_err_table() {
    let (_store, dispatch) = make_stub_dispatch();
    let mut b = Bridge::new(dispatch);
    let reply = b.eval(b"return redis.pcall('FAKECMD')", &[], &[]);
    // pcall catches the error and surfaces as RESP error too.
    assert!(reply.starts_with(b"-ERR "));
}

/// The canonical Redlock unlock script — byte-for-byte from the kevy
/// v1.27 ecosystem-survey corpus.
const REDLOCK_UNLOCK: &[u8] = b"\
if redis.call('GET', KEYS[1]) == ARGV[1] then\n\
    return redis.call('DEL', KEYS[1])\n\
else\n\
    return 0\n\
end\n";

#[test]
fn eval_canonical_redlock_unlock_pattern() {
    let (store, dispatch) = make_stub_dispatch();
    store
        .borrow_mut()
        .insert(b"lock:foo".to_vec(), b"token-abc".to_vec());
    let mut b = Bridge::new(dispatch);
    let reply = b.eval(REDLOCK_UNLOCK, &[b"lock:foo"], &[b"token-abc"]);
    assert_eq!(reply, b":1\r\n");
    assert!(!store.borrow().contains_key(&b"lock:foo".to_vec()));
}

#[test]
fn eval_canonical_redlock_unlock_token_mismatch_returns_zero() {
    let (store, dispatch) = make_stub_dispatch();
    store
        .borrow_mut()
        .insert(b"lock:foo".to_vec(), b"someone-else".to_vec());
    let mut b = Bridge::new(dispatch);
    let reply = b.eval(REDLOCK_UNLOCK, &[b"lock:foo"], &[b"my-token"]);
    assert_eq!(reply, b":0\r\n");
    assert!(store.borrow().contains_key(&b"lock:foo".to_vec()));
}

// ─────────────────────────────────────────────────────────────────────
// FlushMode + bridge lifecycle
// ─────────────────────────────────────────────────────────────────────

#[test]
fn script_flush_modes_round_trip() {
    let mut b = Bridge::with_no_dispatch();
    let _ = b.eval(b"return 1", &[], &[]);
    b.script_flush(FlushMode::Sync);
    b.script_flush(FlushMode::Async);
}

#[test]
fn script_exists_empty_returns_empty() {
    let b = Bridge::with_no_dispatch();
    assert!(b.script_exists(&[]).is_empty());
}

#[test]
fn script_load_returns_zeroed_sha1() {
    // P1 stub — real SHA1 lands in P5.
    let mut b = Bridge::with_no_dispatch();
    let sha = b.script_load(b"return 1");
    assert_eq!(sha, [0u8; 20]);
}

#[test]
fn evalsha_unknown_is_noscript_error() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.evalsha([0u8; 20], &[], &[]);
    assert!(reply.starts_with(b"-NOSCRIPT "));
}
