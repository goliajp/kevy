//! cmsgpack stdlib integration tests — pure pack/unpack round trips
//! and byte-format spec checks through Bridge::eval. Lives outside
//! the cmsgpack source file so src/cmsgpack.rs stays under the
//! 500-LOC house rule.

use kevy_lua::Bridge;

fn unwrap_int(reply: &[u8]) -> i64 {
    assert!(
        reply.starts_with(b":"),
        "reply: {:?}",
        String::from_utf8_lossy(reply)
    );
    let end = reply.iter().position(|&b| b == b'\r').unwrap();
    std::str::from_utf8(&reply[1..end]).unwrap().parse().unwrap()
}
fn unwrap_bulk(reply: &[u8]) -> Vec<u8> {
    assert!(
        reply.starts_with(b"$"),
        "reply: {:?}",
        String::from_utf8_lossy(reply)
    );
    let crlf = reply.iter().position(|&b| b == b'\r').unwrap();
    let n: usize = std::str::from_utf8(&reply[1..crlf]).unwrap().parse().unwrap();
    let start = crlf + 2;
    reply[start..start + n].to_vec()
}

#[test]
fn round_trip_integer() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.eval(
        b"local s = cmsgpack.pack(42); return cmsgpack.unpack(s)",
        &[],
        &[],
    );
    assert_eq!(unwrap_int(&reply), 42);
}

#[test]
fn round_trip_string() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.eval(b"return cmsgpack.unpack(cmsgpack.pack('hello'))", &[], &[]);
    assert_eq!(unwrap_bulk(&reply), b"hello".to_vec());
}

#[test]
fn round_trip_array() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.eval(
        b"local t = cmsgpack.unpack(cmsgpack.pack({10, 20, 30})); return t[1] + t[2] + t[3]",
        &[],
        &[],
    );
    assert_eq!(unwrap_int(&reply), 60);
}

#[test]
fn round_trip_map() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.eval(
        b"local t = cmsgpack.unpack(cmsgpack.pack({name='kevy', age=42}))\n\
          return t.name .. ':' .. t.age",
        &[],
        &[],
    );
    assert_eq!(unwrap_bulk(&reply), b"kevy:42".to_vec());
}

#[test]
fn round_trip_nested() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.eval(
        b"local job = {id='j1', data={x=7, y=42}, tags={'a','b','c'}}\n\
          local s = cmsgpack.pack(job)\n\
          local r = cmsgpack.unpack(s)\n\
          return r.id .. ':' .. r.data.x .. ':' .. r.data.y .. ':' .. r.tags[1] .. r.tags[2] .. r.tags[3]",
        &[],
        &[],
    );
    assert_eq!(unwrap_bulk(&reply), b"j1:7:42:abc".to_vec());
}

#[test]
fn round_trip_nil_and_bool() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.eval(
        b"local s = cmsgpack.pack(true, false)\n\
          local a, b = cmsgpack.unpack(s)\n\
          if a == true and b == false then return 1 else return 0 end",
        &[],
        &[],
    );
    assert_eq!(unwrap_int(&reply), 1);
}

#[test]
fn negative_ints_round_trip() {
    let mut b = Bridge::with_no_dispatch();
    let reply = b.eval(
        b"local t = cmsgpack.unpack(cmsgpack.pack({-1, -100, -10000, -1000000}))\n\
          return t[1] + t[2] + t[3] + t[4]",
        &[],
        &[],
    );
    assert_eq!(unwrap_int(&reply), -1010101);
}

#[test]
fn pack_byte_format_check() {
    let mut b = Bridge::with_no_dispatch();
    // 42 as positive fixint = 0x2a → single byte
    assert_eq!(
        unwrap_int(&b.eval(b"return #cmsgpack.pack(42)", &[], &[])),
        1
    );
    // 200 → uint8 (0xcc 0xc8) → 2 bytes
    assert_eq!(
        unwrap_int(&b.eval(b"return #cmsgpack.pack(200)", &[], &[])),
        2
    );
    // 70000 → uint32 (0xce + 4 bytes) → 5 bytes
    assert_eq!(
        unwrap_int(&b.eval(b"return #cmsgpack.pack(70000)", &[], &[])),
        5
    );
}
