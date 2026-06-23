//! cjson stdlib integration tests — pure encode/decode round trips
//! and escape handling through Bridge::eval. Lives outside the cjson
//! source file so src/cjson.rs stays under the 500-LOC house rule.

use kevy_lua::Bridge;

fn unwrap_int(r: &[u8]) -> i64 {
    assert!(r.starts_with(b":"), "{:?}", String::from_utf8_lossy(r));
    let end = r.iter().position(|&b| b == b'\r').unwrap();
    std::str::from_utf8(&r[1..end]).unwrap().parse().unwrap()
}
fn unwrap_bulk(r: &[u8]) -> Vec<u8> {
    assert!(r.starts_with(b"$"), "{:?}", String::from_utf8_lossy(r));
    let crlf = r.iter().position(|&b| b == b'\r').unwrap();
    let n: usize = std::str::from_utf8(&r[1..crlf]).unwrap().parse().unwrap();
    let s = crlf + 2;
    r[s..s + n].to_vec()
}

#[test]
fn encode_primitives() {
    let mut b = Bridge::with_no_dispatch();
    assert_eq!(
        unwrap_bulk(&b.eval(b"return cjson.encode(42)", &[], &[])),
        b"42".to_vec()
    );
    assert_eq!(
        unwrap_bulk(&b.eval(b"return cjson.encode('hello')", &[], &[])),
        b"\"hello\"".to_vec()
    );
    assert_eq!(
        unwrap_bulk(&b.eval(b"return cjson.encode(true)", &[], &[])),
        b"true".to_vec()
    );
    assert_eq!(
        unwrap_bulk(&b.eval(b"return cjson.encode(nil)", &[], &[])),
        b"null".to_vec()
    );
}

#[test]
fn encode_array() {
    let mut b = Bridge::with_no_dispatch();
    assert_eq!(
        unwrap_bulk(&b.eval(b"return cjson.encode({1, 2, 3})", &[], &[])),
        b"[1,2,3]".to_vec()
    );
}

#[test]
fn encode_object() {
    let mut b = Bridge::with_no_dispatch();
    assert_eq!(
        unwrap_bulk(&b.eval(b"return cjson.encode({name='kevy'})", &[], &[])),
        b"{\"name\":\"kevy\"}".to_vec()
    );
}

#[test]
fn decode_primitives() {
    let mut b = Bridge::with_no_dispatch();
    assert_eq!(unwrap_int(&b.eval(b"return cjson.decode('42')", &[], &[])), 42);
    assert_eq!(
        unwrap_bulk(&b.eval(b"return cjson.decode('\"hello\"')", &[], &[])),
        b"hello".to_vec()
    );
    let r = b.eval(
        b"if cjson.decode('true') == true then return 1 else return 0 end",
        &[],
        &[],
    );
    assert_eq!(unwrap_int(&r), 1);
}

#[test]
fn round_trip_nested() {
    let mut b = Bridge::with_no_dispatch();
    let r = b.eval(
        b"local j = {id='job-7', priority=5, tags={'a','b'}}\n\
          local s = cjson.encode(j)\n\
          local r = cjson.decode(s)\n\
          return r.id .. ':' .. r.priority .. ':' .. r.tags[1] .. r.tags[2]",
        &[],
        &[],
    );
    assert_eq!(unwrap_bulk(&r), b"job-7:5:ab".to_vec());
}

#[test]
fn encode_string_escapes() {
    let mut b = Bridge::with_no_dispatch();
    assert_eq!(
        unwrap_bulk(&b.eval(b"return cjson.encode('a\"b\\\\c\\nd')", &[], &[])),
        b"\"a\\\"b\\\\c\\nd\"".to_vec()
    );
}

#[test]
fn decode_string_escapes() {
    let mut b = Bridge::with_no_dispatch();
    let r = b.eval(b"return cjson.decode('\"a\\\\nb\"')", &[], &[]);
    assert_eq!(unwrap_bulk(&r), b"a\nb".to_vec());
}

#[test]
fn decode_unicode_escape() {
    let mut b = Bridge::with_no_dispatch();
    // Lua source needs double-backslash so Lua 5.1's lexer doesn't
    // reject \u itself; the resulting Lua string contains
    // `"A"` which our cjson decoder maps to 'A'.
    let r = b.eval(b"return cjson.decode('\"\\\\u0041\"')", &[], &[]);
    assert_eq!(unwrap_bulk(&r), b"A".to_vec());
}
