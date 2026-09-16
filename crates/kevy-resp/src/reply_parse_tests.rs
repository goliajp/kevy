//! Tests for `reply_parse`, kept apart to hold the parser file under 500 lines.
use super::*;

#[test]
fn parse_replies() {
    let r = |b: &[u8]| parse_reply(b).unwrap().unwrap().0;
    assert_eq!(r(b"+OK\r\n"), Reply::Simple(b"OK".to_vec()));
    assert_eq!(r(b"-ERR bad\r\n"), Reply::Error(b"ERR bad".to_vec()));
    assert_eq!(r(b":42\r\n"), Reply::Int(42));
    assert_eq!(r(b"$5\r\nhello\r\n"), Reply::Bulk(b"hello".to_vec()));
    assert_eq!(r(b"$-1\r\n"), Reply::Nil);
    assert_eq!(r(b"*-1\r\n"), Reply::Nil);

    let (arr, used) = parse_reply(b"*2\r\n:1\r\n$2\r\nhi\r\n").unwrap().unwrap();
    assert_eq!(arr, Reply::Array(vec![Reply::Int(1), Reply::Bulk(b"hi".to_vec())]));
    assert_eq!(used, 16);

    // Incomplete replies ask for more bytes.
    assert_eq!(parse_reply(b"$5\r\nhel").unwrap(), None);
    assert_eq!(parse_reply(b"*2\r\n:1\r\n").unwrap(), None);
    // RESP3 `!N\r\n...` (blob error) IS a valid prefix now — verify the
    // old "unknown prefix" test moved to a genuinely unknown byte.
    assert!(parse_reply(b"@huh\r\n").is_err());
}

#[test]
fn parse_resp3_scalars() {
    let r = |b: &[u8]| parse_reply(b).unwrap().unwrap().0;
    assert_eq!(r(b"_\r\n"), Reply::Null);
    assert_eq!(r(b"#t\r\n"), Reply::Boolean(true));
    assert_eq!(r(b"#f\r\n"), Reply::Boolean(false));
    assert_eq!(r(b",1.5\r\n"), Reply::Double(1.5));
    assert_eq!(r(b",inf\r\n"), Reply::Double(f64::INFINITY));
    assert_eq!(r(b",-inf\r\n"), Reply::Double(f64::NEG_INFINITY));
    // NaN doesn't satisfy `PartialEq` — match manually.
    match r(b",nan\r\n") {
        Reply::Double(v) => assert!(v.is_nan()),
        other => panic!("expected Double(nan), got {other:?}"),
    }
    assert_eq!(
        r(b"(170141183460469231731687303715884105727\r\n"),
        Reply::BigNumber(b"170141183460469231731687303715884105727".to_vec())
    );
    assert_eq!(r(b"!11\r\nERR bad cmd\r\n"), Reply::BlobError(b"ERR bad cmd".to_vec()));
}

#[test]
fn parse_resp3_verbatim() {
    let r = |b: &[u8]| parse_reply(b).unwrap().unwrap().0;
    assert_eq!(
        r(b"=15\r\ntxt:Some string\r\n"),
        Reply::Verbatim { fmt: *b"txt", data: b"Some string".to_vec() }
    );
    // len < 4 (no room for fmt + ':') is rejected.
    assert!(parse_reply(b"=3\r\ntxt\r\n").is_err());
    // Missing `:` separator is rejected.
    assert!(parse_reply(b"=7\r\ntxt+abc\r\n").is_err());
}

#[test]
fn parse_resp3_map_and_set() {
    let r = |b: &[u8]| parse_reply(b).unwrap().unwrap().0;
    // %2\r\n :1\r\n $1\r\n a\r\n :2\r\n $1\r\n b\r\n
    let m = r(b"%2\r\n:1\r\n$1\r\na\r\n:2\r\n$1\r\nb\r\n");
    assert_eq!(
        m,
        Reply::Map(vec![
            (Reply::Int(1), Reply::Bulk(b"a".to_vec())),
            (Reply::Int(2), Reply::Bulk(b"b".to_vec())),
        ])
    );
    // ~3\r\n :1\r\n :2\r\n :3\r\n
    let s = r(b"~3\r\n:1\r\n:2\r\n:3\r\n");
    assert_eq!(s, Reply::Set(vec![Reply::Int(1), Reply::Int(2), Reply::Int(3)]));
    // Empty map / set.
    assert_eq!(r(b"%0\r\n"), Reply::Map(vec![]));
    assert_eq!(r(b"~0\r\n"), Reply::Set(vec![]));
    // Negative count is malformed (only `*` / `$` allow -1 for nil).
    assert!(parse_reply(b"%-1\r\n").is_err());
    assert!(parse_reply(b"~-1\r\n").is_err());
}

#[test]
fn parse_resp3_push_frame() {
    let r = |b: &[u8]| parse_reply(b).unwrap().unwrap().0;
    let push = r(b">3\r\n+message\r\n$4\r\nnews\r\n$5\r\nhello\r\n");
    assert_eq!(
        push,
        Reply::Push(vec![
            Reply::Simple(b"message".to_vec()),
            Reply::Bulk(b"news".to_vec()),
            Reply::Bulk(b"hello".to_vec()),
        ])
    );
    // Push frames have no null shape.
    assert!(parse_reply(b">-1\r\n").is_err());
}

#[test]
fn parse_resp3_attributes_are_skipped() {
    // |1\r\n +key-popularity\r\n %2\r\n $1\r\n a\r\n ,0.5\r\n $1\r\n b\r\n ,0.3\r\n
    // followed by the actual reply: *2\r\n :1\r\n :2\r\n
    let frame =
        b"|1\r\n+key-popularity\r\n%2\r\n$1\r\na\r\n,0.5\r\n$1\r\nb\r\n,0.3\r\n*2\r\n:1\r\n:2\r\n";
    let (r, used) = parse_reply(frame).unwrap().unwrap();
    assert_eq!(r, Reply::Array(vec![Reply::Int(1), Reply::Int(2)]));
    assert_eq!(used, frame.len());
}

#[test]
fn parse_resp3_partial_returns_none() {
    // Each new shape: cut at every CRLF boundary and assert None.
    for cut in &[b"_".as_slice(), b"_\r", b"#t", b"#t\r"] {
        assert_eq!(parse_reply(cut).unwrap(), None);
    }
    assert_eq!(parse_reply(b"=15\r\ntxt:Some str").unwrap(), None);
    // Map mid-frame.
    assert_eq!(parse_reply(b"%2\r\n:1\r\n$1\r\na\r\n:2\r\n").unwrap(), None);
}

/// The text comes back as the server wrote it, depth-first, including a
/// double inside an attribute-decorated reply; the parsed replies agree
/// with `parse_reply` byte for byte.
#[test]
fn double_text_is_kept_in_walk_order() {
    let wire: &[u8] = b"*3\r\n,1e+300\r\n%1\r\n,0.000001\r\n,inf\r\n|1\r\n+k\r\n+v\r\n,-0\r\n";
    let (reply, used, texts) = parse_reply_keeping_double_text(wire).unwrap().unwrap();
    assert_eq!(used, wire.len());
    assert_eq!(parse_reply(wire).unwrap().unwrap(), (reply, used));
    assert_eq!(texts, [b"1e+300".to_vec(), b"0.000001".to_vec(), b"inf".to_vec(), b"-0".to_vec()]);
    assert_eq!(parse_reply_keeping_double_text(b"*2\r\n,1\r\n").unwrap(), None);
}
