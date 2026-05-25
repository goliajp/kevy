use super::*;

/// Dispatch a command (given as argv pieces) against `store`, returning RESP.
fn d(store: &mut Store, parts: &[&[u8]]) -> Vec<u8> {
    let args: Vec<Vec<u8>> = parts.iter().map(|p| p.to_vec()).collect();
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
    assert_eq!(d(&mut s, &[b"CONFIG", b"GET", b"maxmemory"]), b"*0\r\n");
    assert_eq!(d(&mut s, &[b"CONFIG", b"SET", b"x", b"y"]), b"+OK\r\n");
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
    assert!(matches!(
        c.route(&[b"GET".to_vec(), b"k".to_vec()]),
        Route::Single(1)
    ));
    assert!(matches!(c.route(&[b"PING".to_vec()]), Route::Local));
    assert!(matches!(c.route(&[b"DBSIZE".to_vec()]), Route::Dbsize));
    assert!(matches!(c.route(&[b"FLUSHALL".to_vec()]), Route::Flush));
    assert!(matches!(c.route(&[b"SAVE".to_vec()]), Route::Save));
    // DEL: single key fast path vs multi-key fan-out.
    assert!(matches!(
        c.route(&[b"DEL".to_vec(), b"a".to_vec()]),
        Route::Single(1)
    ));
    assert!(matches!(
        c.route(&[b"DEL".to_vec(), b"a".to_vec(), b"b".to_vec()]),
        Route::DelKeys
    ));
    assert!(c.is_write(&[b"set".to_vec(), b"k".to_vec(), b"v".to_vec()]));
    assert!(!c.is_write(&[b"GET".to_vec(), b"k".to_vec()]));
    assert!(c.is_quit(&[b"quit".to_vec()]));
}
