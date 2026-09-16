//! Every reply type through every output mode. The expected bytes are
//! redis-cli 8.10.1's (read in its formatters and confirmed by
//! `bench/cligate.py`), except where a deviation id says otherwise.

use super::{Delims, Output, is_verbatim_command, render};
use kevy_resp::Reply;

fn delims() -> Delims {
    Delims { multibulk: b"\n".to_vec(), reply: b"\n".to_vec() }
}

fn show(r: &Reply, texts: &[&str], mode: Output) -> String {
    let texts: Vec<Vec<u8>> = texts.iter().map(|t| t.as_bytes().to_vec()).collect();
    String::from_utf8_lossy(&render(r, &texts, mode, &delims(), false)).into_owned()
}

fn bulk(s: &str) -> Reply {
    Reply::Bulk(s.as_bytes().to_vec())
}

#[test]
fn standard_scalars() {
    let s = |r: &Reply| show(r, &[], Output::Standard);
    assert_eq!(s(&Reply::Simple(b"OK".to_vec())), "OK\n");
    assert_eq!(s(&Reply::Error(b"ERR x\0hidden".to_vec())), "(error) ERR x\n");
    assert_eq!(s(&Reply::BlobError(b"ERR y".to_vec())), "(error) ERR y\n");
    assert_eq!(s(&Reply::Int(-3)), "(integer) -3\n");
    assert_eq!(s(&bulk("a\nb\"")), "\"a\\nb\\\"\"\n");
    assert_eq!(s(&Reply::Nil), "(nil)\n");
    assert_eq!(s(&Reply::Null), "(nil)\n");
    assert_eq!(s(&Reply::Boolean(true)), "(true)\n");
    assert_eq!(s(&Reply::Boolean(false)), "(false)\n");
    assert_eq!(s(&Reply::Verbatim { fmt: *b"txt", data: b"line\r\n".to_vec() }), "line\r\n\n");
    assert_eq!(s(&Reply::BigNumber(b"123".to_vec())), "(bignum) 123\n");
    assert_eq!(show(&Reply::Double(1e300), &["1e+300"], Output::Standard), "(double) 1e+300\n");
    // A double parsed without its text still renders, from the value.
    assert_eq!(s(&Reply::Double(2.5)), "(double) 2.5\n");
}

#[test]
fn standard_aggregates() {
    let s = |r: &Reply| show(r, &[], Output::Standard);
    assert_eq!(s(&Reply::Array(vec![])), "(empty array)\n");
    assert_eq!(s(&Reply::Map(vec![])), "(empty hash)\n");
    assert_eq!(s(&Reply::Set(vec![])), "(empty set)\n");
    assert_eq!(s(&Reply::Push(vec![])), "(empty push)\n");
    let ten: Vec<Reply> = (1..=10).map(Reply::Int).collect();
    assert!(s(&Reply::Array(ten)).starts_with(" 1) (integer) 1\n 2) (integer) 2\n"));
    let nested = Reply::Array(vec![bulk("0"), Reply::Array(vec![bulk("a"), bulk("b")])]);
    assert_eq!(s(&nested), "1) \"0\"\n2) 1) \"a\"\n   2) \"b\"\n");
    assert_eq!(s(&Reply::Set(vec![bulk("x")])), "1~ \"x\"\n");
    let map = Reply::Map(vec![
        (bulk("k"), bulk("v")),
        (bulk("l"), Reply::Array(vec![bulk("a"), bulk("b")])),
    ]);
    assert_eq!(s(&map), "1# \"k\" => \"v\"\n2# \"l\" => \n   1) \"a\"\n   2) \"b\"\n");
    let single = Reply::Map(vec![(bulk("k"), Reply::Array(vec![bulk("a")]))]);
    assert_eq!(s(&single), "1# \"k\" => 1) \"a\"\n");
}

#[test]
fn raw_mode() {
    let r = |reply: &Reply, texts: &[&str]| show(reply, texts, Output::Raw);
    assert_eq!(r(&Reply::Error(b"ERR x".to_vec()), &[]), "ERR x\n\n");
    assert_eq!(r(&Reply::Nil, &[]), "\n");
    assert_eq!(r(&Reply::Boolean(true), &[]), "(true)\n");
    assert_eq!(r(&Reply::Int(4), &[]), "4\n");
    assert_eq!(r(&Reply::Double(1.0), &["1"]), "1\n");
    assert_eq!(r(&Reply::BigNumber(b"9".to_vec()), &[]), "9\n");
    let map = Reply::Map(vec![
        (bulk("k1"), bulk("v1")),
        (bulk("k2"), Reply::Array(vec![bulk("a"), bulk("b")])),
    ]);
    assert_eq!(r(&map, &[]), "k1 v1\nk2 a\nb\n");
    let with_delims = Delims { multibulk: b",".to_vec(), reply: b"|".to_vec() };
    let out =
        render(&Reply::Set(vec![bulk("a"), bulk("b")]), &[], Output::Raw, &with_delims, false);
    assert_eq!(out, b"a,b|");
    // Verbatim-raw commands get no reply delimiter, in any mode.
    assert_eq!(render(&bulk("doc"), &[], Output::Csv, &with_delims, true), b"doc");
}

#[test]
fn csv_mode() {
    let c = |reply: &Reply, texts: &[&str]| show(reply, texts, Output::Csv);
    assert_eq!(c(&Reply::Error(b"ERR x".to_vec()), &[]), "ERROR,\"ERR x\"\n");
    assert_eq!(c(&Reply::Simple(b"OK".to_vec()), &[]), "\"OK\"\n");
    assert_eq!(c(&Reply::Nil, &[]), "NULL\n");
    assert_eq!(c(&Reply::Boolean(false), &[]), "false\n");
    assert_eq!(c(&Reply::Double(0.5), &["0.5"]), "0.5\n");
    assert_eq!(c(&Reply::BigNumber(b"7".to_vec()), &[]), "7\n");
    assert_eq!(c(&Reply::Int(1), &[]), "1\n");
    let map =
        Reply::Map(vec![(bulk("a"), Reply::Int(1)), (bulk("b"), Reply::Array(vec![bulk("c")]))]);
    assert_eq!(c(&map, &[]), "\"a\",1,\"b\",\"c\"\n");
}

#[test]
fn json_modes() {
    let j = |reply: &Reply, texts: &[&str]| show(reply, texts, Output::Json);
    // DEV-004 and DEV-009.
    assert_eq!(j(&Reply::Error(b"ERR x".to_vec()), &[]), "{\"error\":\"ERR x\"}\n");
    assert_eq!(j(&Reply::Double(f64::INFINITY), &["inf"]), "\"inf\"\n");
    assert_eq!(j(&Reply::Double(1.5), &["1.5"]), "1.5\n");
    assert_eq!(j(&bulk("a\n\x01\x0c\t\x08\r/\u{e9}"), &[]), "\"a\\n\\u0001\\f\\t\\b\\r/\u{e9}\"\n");
    assert_eq!(j(&Reply::Nil, &[]), "null\n");
    assert_eq!(j(&Reply::Boolean(true), &[]), "true\n");
    assert_eq!(j(&Reply::BigNumber(b"12".to_vec()), &[]), "12\n");
    assert_eq!(j(&Reply::Push(vec![Reply::Int(1), Reply::Null]), &[]), "[1,null]\n");
    let map = Reply::Map(vec![
        (Reply::Int(1), Reply::Boolean(true)),
        (Reply::Error(b"e".to_vec()), bulk("v")),
    ]);
    assert_eq!(j(&map, &[]), "{\"1\":true,\"e\":\"v\"}\n");
    let q = |reply: &Reply| show(reply, &[], Output::QuotedJson);
    // DEV-005: the repr, escaped once.
    assert_eq!(q(&bulk("a\"b")), "\"a\\\\\\\"b\"\n");
    assert_eq!(q(&bulk("a\nb\u{ff}")), "\"a\\\\nb\\\\xc3\\\\xbf\"\n");
}

#[test]
fn invalidate_push_renders_keys() {
    let push = Reply::Push(vec![bulk("invalidate"), Reply::Array(vec![bulk("k1"), bulk("k2")])]);
    assert!(super::is_invalidate(&push));
    assert_eq!(super::invalidate_tty(&push), b"-> invalidate: 'k1', 'k2'\n");
    assert!(!super::is_invalidate(&Reply::Push(vec![bulk("message")])));
}

#[test]
fn verbatim_command_list() {
    let v = |words: &[&str]| {
        is_verbatim_command(&words.iter().map(|w| w.as_bytes().to_vec()).collect::<Vec<_>>())
    };
    assert!(v(&["INFO"]) && v(&["info", "server"]) && v(&["LOLWUT"]));
    assert!(v(&["memory", "doctor"]) && v(&["CLIENT", "LIST"]) && v(&["cluster", "nodes"]));
    assert!(v(&["latency", "graph", "x"]) && v(&["latency", "doctor"]) && v(&["proxy", "info"]));
    assert!(v(&["debug", "htstats", "0"]) && v(&["debug", "client-eviction"]));
    assert!(!v(&["cluster", "nodes", "extra"]) && !v(&["latency", "graph"]) && !v(&["GET", "k"]));
}

#[test]
fn a_push_prints_in_the_session_output_mode() {
    // Pushes print only when a server sends one unprompted (client-side
    // caching); this pins the printer without a server that does.
    let mut opts = super::super::opts::Opts::defaults(true);
    opts.output = Output::Standard;
    let mut session = super::super::session::Session::new(opts);
    let invalidate = Reply::Push(vec![bulk("invalidate"), Reply::Array(vec![bulk("k")])]);
    assert_eq!(session.push_bytes(&invalidate, &[]), b"-> invalidate: 'k'\n");
    session.opts.output = Output::Raw;
    assert_eq!(session.push_bytes(&invalidate, &[]), b"invalidate\nk\n");
}

/// The second half of every `A | B` arm and the aggregate shapes the
/// examples above do not reach, per mode.
#[test]
fn every_arm_alternative() {
    let blob = Reply::BlobError(b"ERR blob".to_vec());
    let verb = Reply::Verbatim { fmt: *b"txt", data: b"v".to_vec() };
    let simple = Reply::Simple(b"S".to_vec());
    assert_eq!(show(&blob, &[], Output::Raw), "ERR blob\n\n");
    assert_eq!(show(&verb, &[], Output::Raw), "v\n");
    assert_eq!(show(&Reply::Boolean(false), &[], Output::Raw), "(false)\n");
    assert_eq!(show(&blob, &[], Output::Csv), "ERROR,\"ERR blob\"\n");
    assert_eq!(show(&verb, &[], Output::Csv), "\"v\"\n");
    assert_eq!(show(&Reply::Boolean(true), &[], Output::Csv), "true\n");
    assert_eq!(
        show(&Reply::Push(vec![simple.clone(), Reply::Int(2)]), &[], Output::Csv),
        "\"S\",2\n"
    );
    assert_eq!(show(&blob, &[], Output::Json), "{\"error\":\"ERR blob\"}\n");
    assert_eq!(show(&verb, &[], Output::Json), "\"v\"\n");
    assert_eq!(show(&simple, &[], Output::Json), "\"S\"\n");
    assert_eq!(show(&Reply::Boolean(false), &[], Output::Json), "false\n");
    assert_eq!(show(&Reply::Set(vec![Reply::Int(1)]), &[], Output::Json), "[1]\n");
    let keys = Reply::Map(vec![
        (verb.clone(), Reply::Int(1)),
        (blob.clone(), Reply::Int(2)),
        (Reply::Double(f64::NAN), Reply::Int(3)),
    ]);
    assert_eq!(show(&keys, &["nan"], Output::Json), "{\"v\":1,\"ERR blob\":2,\"nan\":3}\n");
    // Multi-line detection through every aggregate shape.
    let pairs = |n: i64| Reply::Map((0..n).map(|i| (Reply::Int(i), Reply::Int(i))).collect());
    let nest = |value: Reply| Reply::Map(vec![(bulk("k"), value)]);
    assert_eq!(show(&nest(pairs(0)), &[], Output::Standard), "1# \"k\" => (empty hash)\n");
    assert_eq!(
        show(&nest(pairs(1)), &[], Output::Standard),
        "1# \"k\" => 1# (integer) 0 => (integer) 0\n"
    );
    assert!(show(&nest(pairs(2)), &[], Output::Standard).starts_with("1# \"k\" => \n   1# "));
    assert_eq!(show(&nest(Reply::Set(vec![])), &[], Output::Standard), "1# \"k\" => (empty set)\n");
    let nested_single = nest(Reply::Push(vec![Reply::Array(vec![Reply::Int(1), Reply::Int(2)])]));
    assert!(show(&nested_single, &[], Output::Standard).starts_with("1# \"k\" => \n"));
    // Invalidations whose shape is off are ordinary pushes.
    assert!(!super::is_invalidate(&Reply::Push(vec![bulk("invalidate"), bulk("k")])));
    let odd_key = Reply::Push(vec![bulk("invalidate"), Reply::Array(vec![Reply::Int(1)])]);
    assert_eq!(super::invalidate_tty(&odd_key), b"-> invalidate: ''\n");
    assert!(is_verbatim_command(&[b"CLUSTER".to_vec(), b"INFO".to_vec()]));
}

#[test]
fn remaining_shapes() {
    assert_eq!(show(&Reply::Set(vec![bulk("a")]), &[], Output::Csv), "\"a\"\n");
    let keys = Reply::Map(vec![(Reply::Simple(b"s".to_vec()), Reply::Double(f64::NEG_INFINITY))]);
    assert_eq!(show(&keys, &["-inf"], Output::Json), "{\"s\":\"-inf\"}\n");
    assert_eq!(super::invalidate_tty(&Reply::Array(vec![])), b"-> invalidate: \n");
    assert_eq!(super::super::cnum::strtod_full(b"\xff1"), None);
}
