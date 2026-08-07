//! Probe-transcribed tests. Every expectation here is copied from a
//! pg_regress corpus file (cited per block) — the same files funcgate
//! replays end to end. Where a case looks strange, the probe header
//! explains why PG chose it; nothing here is our own invention.

use crate::{Scalar, ScalarError, eval};

fn t(s: &str) -> Scalar {
    Scalar::Text(s.to_string())
}

fn txt(v: Result<Scalar, ScalarError>) -> String {
    match v.unwrap() {
        Scalar::Text(s) => s,
        Scalar::Null => "NULL".into(),
        other => panic!("expected text, got {other:?}"),
    }
}

fn int(v: Result<Scalar, ScalarError>) -> i64 {
    match v.unwrap() {
        Scalar::Int(i) => i,
        other => panic!("expected int, got {other:?}"),
    }
}

fn flt(v: Result<Scalar, ScalarError>) -> f64 {
    match v.unwrap() {
        Scalar::Float(f) => f,
        Scalar::Int(i) => i as f64,
        other => panic!("expected number, got {other:?}"),
    }
}

// ── probe 39: trim family — char SET, default space only ──
#[test]
fn trim_is_a_char_set_with_space_default() {
    assert_eq!(txt(eval("trim", &[t("  hello  ")])), "hello");
    assert_eq!(txt(eval("trim", &[t("\thello\n")])), "\thello\n"); // tab/nl survive
    assert_eq!(txt(eval("btrim", &[t("xyxhelloxyx"), t("xy")])), "hello");
    assert_eq!(txt(eval("ltrim", &[t("  hi ")])), "hi ");
    assert_eq!(txt(eval("rtrim", &[t(" hi  ")])), " hi");
    assert_eq!(eval("trim", &[Scalar::Null]).unwrap(), Scalar::Null);
}

// ── probe 41: split_part — 1-indexed, negative from end ──
#[test]
fn split_part_indexing_both_directions() {
    let sp = |s: &str, d: &str, n: i64| txt(eval("split_part", &[t(s), t(d), Scalar::Int(n)]));
    assert_eq!(sp("a,b,c", ",", 1), "a");
    assert_eq!(sp("a,b,c", ",", 3), "c");
    assert_eq!(sp("a,b,c", ",", 4), "");
    assert_eq!(sp("a,b,c", ",", -1), "c");
    assert_eq!(sp("a,b,c", ",", -3), "a");
    assert_eq!(sp("a,b,c", ",", -99), "");
    assert_eq!(sp("abc", "", 1), "abc"); // empty delim = whole string
    assert_eq!(sp("a,,b", ",", 2), ""); // empty fields preserved
    assert_eq!(sp("a::b::c", "::", 2), "b");
    assert_eq!(sp("foo.tar.gz", ".", -1), "gz");
    assert!(matches!(
        eval("split_part", &[t("a,b"), t(","), Scalar::Int(0)]),
        Err(ScalarError::Domain { .. })
    ));
}

// ── probe 43: lpad/rpad — truncate keeps the left, fill cycles ──
#[test]
fn pads_truncate_left_and_cycle_fill() {
    assert_eq!(txt(eval("lpad", &[t("hi"), Scalar::Int(5)])), "   hi");
    assert_eq!(txt(eval("rpad", &[t("hi"), Scalar::Int(5), t("ab")])), "hiaba");
    assert_eq!(txt(eval("lpad", &[t("abcdef"), Scalar::Int(3)])), "abc");
    assert_eq!(txt(eval("rpad", &[t("abcdef"), Scalar::Int(3)])), "abc");
    assert_eq!(txt(eval("lpad", &[t("hi"), Scalar::Int(0)])), "");
    assert_eq!(txt(eval("lpad", &[t("hi"), Scalar::Int(5), t("")])), "hi");
}

// ── probe 44: strpos vs position — reversed argument order ──
#[test]
fn strpos_and_position_argument_orders() {
    assert_eq!(int(eval("strpos", &[t("high"), t("ig")])), 2);
    assert_eq!(int(eval("strpos", &[t("high"), t("zz")])), 0);
    assert_eq!(int(eval("strpos", &[t("high"), t("")])), 1);
    assert_eq!(int(eval("position", &[t("ig"), t("high")])), 2);
}

// ── probe 45: left/right — negative counts from the other side ──
#[test]
fn left_right_negative_counts() {
    assert_eq!(txt(eval("left", &[t("hello"), Scalar::Int(2)])), "he");
    assert_eq!(txt(eval("left", &[t("hello"), Scalar::Int(-2)])), "hel");
    assert_eq!(txt(eval("right", &[t("hello"), Scalar::Int(2)])), "lo");
    assert_eq!(txt(eval("right", &[t("hello"), Scalar::Int(-2)])), "llo");
    assert_eq!(txt(eval("left", &[t("hello"), Scalar::Int(0)])), "");
    assert_eq!(txt(eval("left", &[t("hi"), Scalar::Int(99)])), "hi");
}

// ── probes 46-49: the three rounding rules ──
#[test]
fn floor_ceil_round_trunc_disagree_on_negatives() {
    assert_eq!(flt(eval("floor", &[Scalar::Float(1.7)])), 1.0);
    assert_eq!(flt(eval("floor", &[Scalar::Float(-1.5)])), -2.0); // toward -inf
    assert_eq!(flt(eval("ceil", &[Scalar::Float(-1.5)])), -1.0); // toward zero
    assert_eq!(flt(eval("ceiling", &[Scalar::Float(1.1)])), 2.0);
    assert_eq!(flt(eval("round", &[Scalar::Float(2.5)])), 3.0); // half AWAY
    assert_eq!(flt(eval("round", &[Scalar::Float(-2.5)])), -3.0);
    assert_eq!(flt(eval("trunc", &[Scalar::Float(-1.9)])), -1.0); // toward zero
    assert_eq!(flt(eval("round", &[Scalar::Float(2.345), Scalar::Int(2)])), 2.35);
    assert_eq!(flt(eval("round", &[Scalar::Float(1234.5), Scalar::Int(-2)])), 1200.0);
    assert_eq!(int(eval("floor", &[Scalar::Int(-3)])), -3); // int passthrough
}

// ── probes 50-51: null family ──
#[test]
fn null_family_looks_through() {
    assert_eq!(txt(eval("coalesce", &[Scalar::Null, t("x")])), "x");
    assert_eq!(eval("coalesce", &[Scalar::Null, Scalar::Null]).unwrap(), Scalar::Null);
    assert_eq!(eval("nullif", &[t("a"), t("a")]).unwrap(), Scalar::Null);
    assert_eq!(txt(eval("nullif", &[t("a"), t("b")])), "a");
    assert_eq!(int(eval("greatest", &[Scalar::Int(1), Scalar::Null, Scalar::Int(3)])), 3);
    assert_eq!(int(eval("least", &[Scalar::Int(1), Scalar::Null, Scalar::Int(3)])), 1);
    assert_eq!(eval("greatest", &[Scalar::Null, Scalar::Null]).unwrap(), Scalar::Null);
}

// ── probes 52-55: mod/power/sqrt/sign ──
#[test]
fn mod_power_sqrt_sign_domains() {
    assert_eq!(int(eval("mod", &[Scalar::Int(-7), Scalar::Int(3)])), -1); // dividend sign
    assert!(matches!(
        eval("mod", &[Scalar::Int(1), Scalar::Int(0)]),
        Err(ScalarError::Domain { .. })
    ));
    assert_eq!(int(eval("power", &[Scalar::Int(2), Scalar::Int(10)])), 1024);
    assert!(matches!(
        eval("power", &[Scalar::Int(0), Scalar::Int(-1)]),
        Err(ScalarError::Domain { .. })
    ));
    assert!(matches!(
        eval("sqrt", &[Scalar::Int(-1)]),
        Err(ScalarError::Domain { .. })
    ));
    assert_eq!(flt(eval("sqrt", &[Scalar::Int(9)])), 3.0);
    assert_eq!(int(eval("sign", &[Scalar::Float(-4.2)])), -1);
}

// ── probes 36-37: concat family NULL rules ──
#[test]
fn concat_skips_null_but_null_separator_poisons() {
    assert_eq!(txt(eval("concat", &[t("a"), Scalar::Null, t("b"), Scalar::Int(7)])), "ab7");
    assert_eq!(
        txt(eval("concat_ws", &[t("-"), t("a"), Scalar::Null, t("b")])),
        "a-b" // no doubled separator around the skipped NULL
    );
    assert_eq!(
        eval("concat_ws", &[Scalar::Null, t("a"), t("b")]).unwrap(),
        Scalar::Null
    );
}

// ── probe 57 + case/measure misc ──
#[test]
fn translate_case_and_measures() {
    assert_eq!(txt(eval("translate", &[t("12345"), t("14"), t("ax")])), "a23x5");
    assert_eq!(txt(eval("translate", &[t("abcd"), t("bd"), t("x")])), "axc"); // extra deleted
    assert_eq!(txt(eval("initcap", &[t("hello  WORLD-of sql")])), "Hello  World-Of Sql");
    assert_eq!(int(eval("length", &[t("héllo")])), 5); // codepoints not bytes
    assert_eq!(txt(eval("reverse", &[t("abc")])), "cba");
    assert_eq!(txt(eval("repeat", &[t("ab"), Scalar::Int(3)])), "ababab");
    assert_eq!(txt(eval("repeat", &[t("ab"), Scalar::Int(-1)])), "");
    assert_eq!(txt(eval("replace", &[t("aaa"), t("aa"), t("b")])), "ba"); // no re-scan
    assert_eq!(txt(eval("replace", &[t("abc"), t(""), t("x")])), "abc");
    assert_eq!(txt(eval("substr", &[t("hello"), Scalar::Int(2), Scalar::Int(3)])), "ell");
    assert_eq!(txt(eval("substring", &[t("hello"), Scalar::Int(-1), Scalar::Int(3)])), "h");
    assert_eq!(txt(eval("upper", &[t("hi")])), "HI");
}

// ── the load-bearing error: unknown functions carry their name ──
#[test]
fn unknown_function_is_named() {
    let Err(ScalarError::UnknownFunction(name)) = eval("no_such_fn", &[]) else {
        panic!("expected UnknownFunction");
    };
    assert_eq!(name, "no_such_fn");
    assert_eq!(format!("{}", ScalarError::UnknownFunction(name)), "unknown function: no_such_fn");
}
