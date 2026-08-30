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
    assert!(matches!(eval("sqrt", &[Scalar::Int(-1)]), Err(ScalarError::Domain { .. })));
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
    assert_eq!(eval("concat_ws", &[Scalar::Null, t("a"), t("b")]).unwrap(), Scalar::Null);
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

// ── probes 07/10/11: date/time family ──

fn ts(s: &str) -> Scalar {
    Scalar::Timestamp(crate::parse_timestamp(s).expect("valid timestamp literal"))
}

#[test]
fn extract_and_date_part_field_matrices() {
    let t1 = ts("2024-03-15 10:20:45.123456");
    assert_eq!(flt(eval("extract", &[t("year"), t1.clone()])), 2024.0);
    assert_eq!(flt(eval("extract", &[t("month"), t1.clone()])), 3.0);
    assert_eq!(flt(eval("extract", &[t("hour"), t1.clone()])), 10.0);
    assert_eq!(flt(eval("extract", &[t("second"), t1.clone()])), 45.123456);
    // extract refuses time fields on DATE; date_part promotes to 0.
    let d = Scalar::Date(crate::parse_date("2030-06-15").unwrap());
    assert_eq!(flt(eval("extract", &[t("year"), d.clone()])), 2030.0);
    assert!(matches!(eval("extract", &[t("hour"), d.clone()]), Err(ScalarError::Domain { .. })));
    assert_eq!(flt(eval("date_part", &[t("hour"), d])), 0.0);
    // Interval decomposition (probe 11).
    let iv = Scalar::Interval { months: 17, days: 0, micros: 0 };
    assert_eq!(flt(eval("date_part", &[t("month"), iv.clone()])), 5.0);
    assert_eq!(flt(eval("date_part", &[t("year"), iv])), 1.0);
    assert!(matches!(
        eval("date_part", &[t("fortnight"), ts("2024-01-01")]),
        Err(ScalarError::Domain { .. })
    ));
    assert_eq!(eval("date_part", &[t("year"), Scalar::Null]).unwrap(), Scalar::Null);
}

#[test]
fn date_trunc_boundaries() {
    let t1 = ts("2024-03-15 14:30:45");
    let out = eval("date_trunc", &[t("day"), t1.clone()]).unwrap();
    assert_eq!(
        crate::render_timestamp(match out {
            Scalar::Timestamp(us) => us,
            other => panic!("{other:?}"),
        }),
        "2024-03-15 00:00:00"
    );
    let out = eval("date_trunc", &[t("month"), t1.clone()]).unwrap();
    assert_eq!(out, ts("2024-03-01 00:00:00"));
    let out = eval("date_trunc", &[t("hour"), t1]).unwrap();
    assert_eq!(out, ts("2024-03-15 14:00:00"));
    // ISO week starts Monday: 2024-03-15 is a Friday.
    let out = eval("date_trunc", &[t("week"), ts("2024-03-15 14:30:45")]).unwrap();
    assert_eq!(out, ts("2024-03-11 00:00:00"));
}

#[test]
fn interval_parse_and_render_round_trip() {
    assert_eq!(crate::parse_interval("1 day"), Some((0, 1, 0)));
    assert_eq!(crate::parse_interval("1 year 2 months"), Some((14, 0, 0)));
    assert_eq!(crate::render_interval(0, 1, 0), "1 day");
    assert_eq!(crate::render_interval(0, 0, 7_200_000_000), "02:00:00");
    assert_eq!(crate::render_interval(14, 0, 0), "1 year 2 mons");
    assert_eq!(crate::render_interval(0, -3, 0), "-3 days");
    assert_eq!(crate::render_interval(0, 0, 0), "00:00:00");
    // Components never normalize across each other (probe 10).
    assert_eq!(crate::render_interval(0, 1, -12 * 3_600_000_000), "1 day -12:00:00");
}

#[test]
fn age_decomposes_calendar_months_first() {
    let out = eval("age", &[ts("2025-06-15 00:00:00"), ts("2024-03-10 00:00:00")]).unwrap();
    assert_eq!(out, Scalar::Interval { months: 15, days: 5, micros: 0 });
    // Reversed arguments flip every component's sign.
    let out = eval("age", &[ts("2024-03-10 00:00:00"), ts("2025-06-15 00:00:00")]).unwrap();
    assert_eq!(out, Scalar::Interval { months: -15, days: -5, micros: 0 });
}

#[test]
fn timestamp_parse_render_and_to_char() {
    assert_eq!(
        crate::render_timestamp(crate::parse_timestamp("2024-01-01 00:00:30").unwrap()),
        "2024-01-01 00:00:30"
    );
    assert_eq!(
        crate::render_timestamp(crate::parse_timestamp("2024-03-15 10:20:45.123456").unwrap()),
        "2024-03-15 10:20:45.123456"
    );
    assert_eq!(crate::parse_date("2024-02-30"), None); // round-trip reject
    assert_eq!(crate::parse_timestamp("2024-13-01"), None);
    assert_eq!(
        txt(eval("to_char", &[ts("2024-03-15 14:30:45"), t("YYYY-MM-DD HH24:MI:SS")])),
        "2024-03-15 14:30:45"
    );
    // `Month` graduated from refused to rendered (9-char pad, PG's
    // spelling); an unknown letter still refuses by name.
    assert_eq!(txt(eval("to_char", &[ts("2024-03-15 00:00:00"), t("Month")])), "March    ");
    assert!(matches!(
        eval("to_char", &[ts("2024-03-15 00:00:00"), t("Q")]),
        Err(ScalarError::Domain { .. })
    ));
}

// ── probe 32: format() — %s/%L/%I/%%/positional ──
#[test]
fn format_specifiers_from_probe_32() {
    assert_eq!(txt(eval("format", &[t("Hello %s"), t("world")])), "Hello world");
    assert_eq!(
        txt(eval("format", &[t("%s + %s = %s"), Scalar::Int(1), Scalar::Int(2), Scalar::Int(3)])),
        "1 + 2 = 3"
    );
    assert_eq!(txt(eval("format", &[t("= %L"), t("O'Brien")])), "= 'O''Brien'");
    assert_eq!(txt(eval("format", &[t("= %L"), Scalar::Null])), "= NULL");
    assert_eq!(txt(eval("format", &[t("SELECT FROM %I"), t("mytable")])), "SELECT FROM mytable");
    assert_eq!(txt(eval("format", &[t("100%%")])), "100%");
    assert_eq!(txt(eval("format", &[t("%2$s %1$s"), t("last"), t("first")])), "first last");
    assert_eq!(eval("format", &[Scalar::Null, t("x")]).unwrap(), Scalar::Null);
    assert!(matches!(eval("format", &[t("%q"), t("x")]), Err(ScalarError::Domain { .. })));
    assert!(matches!(
        eval("format", &[t("%s %s"), t("only-one")]),
        Err(ScalarError::Domain { .. })
    ));
}

// ── md5 (RFC 1321) as PG's md5(text) ──
#[test]
fn md5_matches_postgres() {
    // PG: md5('') = the empty digest; lowercase hex; NULL → NULL.
    assert_eq!(txt(eval("md5", &[t("")])), "d41d8cd98f00b204e9800998ecf8427e");
    assert_eq!(txt(eval("md5", &[t("abc")])), "900150983cd24fb0d6963f7d28e17f72");
    assert_eq!(eval("md5", &[Scalar::Null]).unwrap(), Scalar::Null);
    assert!(matches!(eval("md5", &[Scalar::Int(1)]), Err(ScalarError::Type { .. })));
    assert!(matches!(eval("md5", &[t("a"), t("b")]), Err(ScalarError::Arity { .. })));
}

// ── probe 33: regexp family over the vendored engine ──
#[test]
fn regexp_replace_matches_and_split() {
    // replace: first vs global, capture-group backrefs, char classes.
    assert_eq!(txt(eval("regexp_replace", &[t("hello world"), t("world"), t("PG")])), "hello PG");
    assert_eq!(txt(eval("regexp_replace", &[t("a b a b"), t("a"), t("X")])), "X b a b");
    assert_eq!(txt(eval("regexp_replace", &[t("a b a b"), t("a"), t("X"), t("g")])), "X b X b");
    assert_eq!(
        txt(eval("regexp_replace", &[t("Hello, World!"), t("[^a-zA-Z0-9]"), t("-"), t("g")])),
        "Hello--World-"
    );
    assert_eq!(txt(eval("regexp_replace", &[t("hello"), t(r"\d+"), t("X")])), "hello");
    assert_eq!(eval("regexp_replace", &[Scalar::Null, t("a"), t("b")]).unwrap(), Scalar::Null);

    // matches: single row renders as a PG {..} array; g / no-match / NULL
    // are set-returning cardinalities the one-row fold face refuses.
    assert_eq!(txt(eval("regexp_matches", &[t("abc123def"), t(r"\d+")])), "{123}");
    assert_eq!(txt(eval("regexp_matches", &[t("hello world"), t("world")])), "{world}");
    assert!(matches!(
        eval("regexp_matches", &[t("a1b22c333"), t(r"\d+"), t("g")]),
        Err(ScalarError::Domain { .. }) // multiple rows
    ));
    assert!(matches!(
        eval("regexp_matches", &[t("hello"), t(r"\d+")]),
        Err(ScalarError::Domain { .. }) // zero rows
    ));
    assert!(matches!(
        eval("regexp_matches", &[Scalar::Null, t(r"\d+")]),
        Err(ScalarError::Domain { .. }) // NULL input → zero rows
    ));

    // split_to_array: literal and whitespace-class delimiters.
    assert_eq!(txt(eval("regexp_split_to_array", &[t("a,b,c"), t(",")])), "{a,b,c}");
    assert_eq!(
        txt(eval("regexp_split_to_array", &[t("one two   three"), t(r"\s+")])),
        "{one,two,three}"
    );
    assert_eq!(txt(eval("regexp_split_to_array", &[t("abc"), t(",")])), "{abc}");

    // A bad pattern is a named refusal, not a wrong answer.
    assert!(matches!(
        eval("regexp_replace", &[t("x"), t("("), t("y")]),
        Err(ScalarError::Domain { .. })
    ));
}

#[test]
fn three_valued_logic_and_comparisons() {
    use crate::{Scalar as S, cmp_op, logic_and, logic_not, logic_or};
    let (t, f, n) = (S::Bool(true), S::Bool(false), S::Null);
    assert_eq!(logic_and(&n, &f).unwrap(), f);
    assert_eq!(logic_and(&n, &t).unwrap(), n);
    assert_eq!(logic_or(&n, &t).unwrap(), t);
    assert_eq!(logic_or(&n, &f).unwrap(), n);
    assert_eq!(logic_not(&n).unwrap(), n);
    assert_eq!(cmp_op("<", &f, &t).unwrap(), S::Bool(true));
    assert_eq!(cmp_op("<>", &S::Int(2), &S::Float(2.5)).unwrap(), S::Bool(true));
    assert_eq!(cmp_op("=", &n, &t).unwrap(), n);
    assert_eq!(cmp_op("=", &S::Timestamp(86_400_000_000), &S::Date(1)).unwrap(), S::Bool(true));
}

#[test]
fn pg_bool_vocabulary() {
    use crate::parse_pg_bool;
    for s in ["t", "TRUE", " yes ", "on", "1"] {
        assert_eq!(parse_pg_bool(s), Some(true), "{s}");
    }
    for s in ["f", "False", "no", "OFF", "0"] {
        assert_eq!(parse_pg_bool(s), Some(false), "{s}");
    }
    for s in ["", "2", "of", "tru e"] {
        assert_eq!(parse_pg_bool(s), None, "{s}");
    }
}

#[test]
fn mysql_time_alias_trio() {
    use crate::{Scalar as S, eval};
    let ts = S::Timestamp(1_749_393_045 * 1_000_000); // 2025-06-08 14:30:45
    let fmt = |tpl: &str| eval("date_format", &[ts.clone(), S::Text(tpl.into())]).unwrap();
    assert_eq!(fmt("%Y-%m-%d %H:%i:%s"), S::Text("2025-06-08 14:30:45".into()));
    assert_eq!(fmt("%M"), S::Text("June".into()));
    assert_eq!(fmt("%b"), S::Text("Jun".into()));
    assert_eq!(fmt("%p"), S::Text("PM".into()));
    assert_eq!(fmt("%Y%%"), S::Text("2025%".into()));
    assert_eq!(eval("unix_timestamp", std::slice::from_ref(&ts)).unwrap(), S::Int(1_749_393_045));
    assert_eq!(
        eval("from_unixtime", &[S::Int(1_749_393_045)]).unwrap(),
        S::Timestamp(1_749_393_045 * 1_000_000)
    );
}

#[test]
fn to_char_extended_patterns() {
    use crate::{Scalar as S, eval};
    let d = S::Date(19_797); // 2024-03-15
    let tc = |v: &S, tpl: &str| eval("to_char", &[v.clone(), S::Text(tpl.into())]).unwrap();
    assert_eq!(tc(&d, "Month DD, YYYY"), S::Text("March     15, 2024".into()));
    assert_eq!(tc(&d, "Mon DD"), S::Text("Mar 15".into()));
    assert_eq!(tc(&d, "YY/MM/DD"), S::Text("24/03/15".into()));
    let noon_ish = S::Timestamp((19_875 * 86_400 + 13 * 3600 + 45 * 60) * 1_000_000);
    assert_eq!(tc(&noon_ish, "HH12:MI PM"), S::Text("01:45 PM".into()));
}

/// A year the arithmetic behind the type cannot hold is not a date.
///
/// Measured before the bound: `parse_date("99999999-01-01")` returned
/// 36,523,530,107 days, and the five sites that multiply a `Date` by
/// MICROS_PER_DAY produced 3.15e21 against an i64 ceiling of 9.22e18 —
/// wrapping in a release build, which sets no `overflow-checks`, and
/// panicking in this one. Twelve digits of year panicked earlier still,
/// inside `epoch_from_civil`, before any check could see it.
#[test]
fn a_date_that_cannot_be_micros_is_not_a_date() {
    use crate::datetime::MICROS_PER_DAY;

    assert!(crate::parse_date("2020-01-01").is_some(), "ordinary dates are untouched");
    assert!(crate::parse_date("1969-12-31").is_some(), "so is before the epoch");
    assert_eq!(crate::parse_date("99999999-01-01"), None, "accepted, and its micros overflowed");
    assert_eq!(crate::parse_date("999999999999-01-01"), None, "panicked inside epoch_from_civil");

    // Whatever the largest accepted year turns out to be, its microseconds
    // must fit — that is the contract, not the constant.
    let mut last = None;
    for y in [200_000i64, 292_000, 292_277, 292_278, 300_000, 999_999] {
        if let Some(days) = crate::parse_date(&format!("{y}-12-31")) {
            assert!(
                days.checked_mul(MICROS_PER_DAY).is_some(),
                "year {y} was accepted but its microseconds overflow"
            );
            last = Some(y);
        }
    }
    assert!(last.is_some(), "the bound rejected every year, including plausible ones");
    assert!(last.unwrap() >= 200_000, "the bound is far tighter than the arithmetic requires");
}
