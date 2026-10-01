//! Wire-level tests for the scalar VALUES / FILTER / SORT / DISTINCT /
//! FACET / OFFSET surface through the embedded dispatcher — the same
//! clause semantics the server e2e pins, byte-for-byte (the oracle test
//! additionally pins server↔embedded on a shared subset).

use kevy_embedded::{Config, Store};

fn store() -> Store {
    Store::open(Config::default().with_ttl_reaper_manual()).expect("open store")
}

fn run(s: &Store, argv: &[&[u8]]) -> Vec<u8> {
    let owned: Vec<Vec<u8>> = argv.iter().map(|a| a.to_vec()).collect();
    let mut out = Vec::new();
    s.dispatch_argv(&owned, &mut out);
    out
}

fn bulk(b: &[u8]) -> String {
    format!("${}\r\n{}\r\n", b.len(), String::from_utf8_lossy(b))
}

/// The flat `[cursor, rows]` reply for keys+values with no clauses'
/// extras: `*2` then cursor `"0"` then `*2N` of key/value bulks.
fn flat_reply(rows: &[(&str, &str)]) -> Vec<u8> {
    let mut out = format!("*2\r\n{}", bulk(b"0"));
    out.push_str(&format!("*{}\r\n", rows.len() * 2));
    for (k, v) in rows {
        out.push_str(&bulk(k.as_bytes()));
        out.push_str(&bulk(v.as_bytes()));
    }
    out.into_bytes()
}

/// Six rows on `v:` — ages 10..60; v:3 has no city, v:4 no price, v:6
/// an uncoercible price. The fixture every assertion below reads from.
fn seeded() -> Store {
    let s = store();
    let rows: &[(&str, &[(&str, &str)])] = &[
        ("v:1", &[("age", "10"), ("city", "tokyo"), ("price", "5")]),
        ("v:2", &[("age", "20"), ("city", "osaka"), ("price", "3")]),
        ("v:3", &[("age", "30"), ("price", "8")]),
        ("v:4", &[("age", "40"), ("city", "tokyo")]),
        ("v:5", &[("age", "50"), ("city", "kyoto"), ("price", "3")]),
        ("v:6", &[("age", "60"), ("city", "osaka"), ("price", "x")]),
    ];
    for (key, fields) in rows {
        let mut argv: Vec<&[u8]> = vec![b"HSET", key.as_bytes()];
        for (f, v) in *fields {
            argv.push(f.as_bytes());
            argv.push(v.as_bytes());
        }
        run(&s, &argv);
    }
    let r = run(
        &s,
        &[
            b"IDX.CREATE",
            b"vals",
            b"ON",
            b"PREFIX",
            b"v:",
            b"FIELD",
            b"age",
            b"TYPE",
            b"i64",
            b"KIND",
            b"range",
            b"VALUES",
            b"city",
            b"price",
            b"TYPES",
            b"str",
            b"i64",
        ],
    );
    assert_eq!(r, b"+OK\r\n");
    s
}

fn q(s: &Store, tail: &[&[u8]]) -> Vec<u8> {
    let mut argv: Vec<&[u8]> = vec![b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100"];
    argv.extend_from_slice(tail);
    run(s, &argv)
}

#[test]
fn plain_range_reply_is_byte_stable_next_to_a_values_free_twin() {
    let s = seeded();
    let r = run(
        &s,
        &[
            b"IDX.CREATE",
            b"plain",
            b"ON",
            b"PREFIX",
            b"v:",
            b"FIELD",
            b"age",
            b"TYPE",
            b"i64",
            b"KIND",
            b"range",
        ],
    );
    assert_eq!(r, b"+OK\r\n");
    let with_values = q(&s, &[]);
    let argv: Vec<&[u8]> = vec![b"IDX.QUERY", b"plain", b"RANGE", b"0", b"100"];
    let without = run(&s, &argv);
    assert_eq!(with_values, without, "declaring VALUES must not move a plain reply's bytes");
    assert_eq!(
        with_values,
        flat_reply(&[
            ("v:1", "10"),
            ("v:2", "20"),
            ("v:3", "30"),
            ("v:4", "40"),
            ("v:5", "50"),
            ("v:6", "60"),
        ])
    );
}

#[test]
fn filter_excludes_failing_and_missing_rows() {
    let s = seeded();
    assert_eq!(
        q(&s, &[b"FILTER", b"city", b"EQ", b"tokyo"]),
        flat_reply(&[("v:1", "10"), ("v:4", "40")]),
        "v:3 has no city — absent is not a value"
    );
    assert_eq!(
        q(&s, &[b"FILTER", b"price", b"RANGE", b"0", b"6"]),
        flat_reply(&[("v:1", "10"), ("v:2", "20"), ("v:5", "50")]),
        "no price (v:4) and uncoercible price (v:6) both fail; 8 (v:3) is out of range"
    );
}

#[test]
fn filter_pages_with_a_cursor() {
    let s = seeded();
    let page1 = q(&s, &[b"FILTER", b"city", b"EQ", b"tokyo", b"LIMIT", b"1"]);
    // Full page → a resume cursor (not "0"); v:1 first in driving order.
    let body = String::from_utf8_lossy(&page1).into_owned();
    assert!(body.contains("v:1") && !body.contains("v:4"), "{body}");
    let cursor_line = body.split("\r\n").nth(2).unwrap().to_owned();
    assert_ne!(cursor_line, "0", "a full filtered page carries a cursor");
    let page2 = q(
        &s,
        &[b"FILTER", b"city", b"EQ", b"tokyo", b"LIMIT", b"5", b"CURSOR", cursor_line.as_bytes()],
    );
    assert_eq!(page2, flat_reply(&[("v:4", "40")]), "the next page resumes past the cursor");
}

#[test]
fn sort_orders_by_the_declared_type_missing_last_both_directions() {
    let s = seeded();
    assert_eq!(
        q(&s, &[b"SORT", b"city", b"ASC"]),
        flat_reply(&[
            ("v:5", "50"),
            ("v:2", "20"),
            ("v:6", "60"),
            ("v:1", "10"),
            ("v:4", "40"),
            ("v:3", "30"),
        ]),
        "kyoto, osaka(key tie), tokyo(key tie), missing LAST"
    );
    assert_eq!(
        q(&s, &[b"SORT", b"city", b"DESC"]),
        flat_reply(&[
            ("v:1", "10"),
            ("v:4", "40"),
            ("v:2", "20"),
            ("v:6", "60"),
            ("v:5", "50"),
            ("v:3", "30"),
        ]),
        "descending flips the valued rows; missing stays LAST"
    );
    assert_eq!(
        q(&s, &[b"SORT", b"price", b"ASC"]),
        flat_reply(&[
            ("v:2", "20"),
            ("v:5", "50"),
            ("v:1", "10"),
            ("v:3", "30"),
            ("v:4", "40"),
            ("v:6", "60"),
        ]),
        "numeric order under TYPES i64; no-price and uncoercible-price both sort last"
    );
}

#[test]
fn distinct_collapses_and_no_value_rows_survive() {
    let s = seeded();
    assert_eq!(
        q(&s, &[b"DISTINCT", b"city"]),
        flat_reply(&[("v:1", "10"), ("v:2", "20"), ("v:3", "30"), ("v:5", "50")]),
        "first per city in driving order; the cityless v:3 is its own group"
    );
}

#[test]
fn facet_counts_the_whole_match_set_and_appends_one_element() {
    let s = seeded();
    let expect = format!(
        "*2\r\n{}*5\r\n{}{}{}{}*2\r\n{}*6\r\n{}{}{}{}{}{}",
        bulk(b"0"),
        bulk(b"v:1"),
        bulk(b"10"),
        bulk(b"v:2"),
        bulk(b"20"),
        bulk(b"city"),
        bulk(b"osaka"),
        bulk(b"2"),
        bulk(b"tokyo"),
        bulk(b"2"),
        bulk(b"kyoto"),
        bulk(b"1"),
    );
    assert_eq!(
        String::from_utf8_lossy(&q(&s, &[b"FACET", b"city", b"LIMIT", b"2"])),
        expect,
        "counts cover the WHOLE match set; the page is still LIMIT rows; ties break by label"
    );
    // FILTER reduces the counts…
    let filtered = q(&s, &[b"FILTER", b"price", b"RANGE", b"0", b"6", b"FACET", b"city"]);
    let body = String::from_utf8_lossy(&filtered).into_owned();
    for (label, n) in [("kyoto", "1"), ("osaka", "1"), ("tokyo", "1")] {
        assert!(body.contains(&format!("{}\r\n$1\r\n{}", label, n)), "{body}");
    }
    // …DISTINCT does not.
    let collapsed = q(&s, &[b"DISTINCT", b"city", b"FACET", b"city"]);
    let body = String::from_utf8_lossy(&collapsed).into_owned();
    assert!(body.contains("osaka\r\n$1\r\n2") && body.contains("tokyo\r\n$1\r\n2"), "{body}");
}

#[test]
fn offset_pages_do_not_overlap_and_past_the_end_is_empty() {
    let s = seeded();
    assert_eq!(
        q(&s, &[b"OFFSET", b"2", b"LIMIT", b"2"]),
        flat_reply(&[("v:3", "30"), ("v:4", "40")])
    );
    assert_eq!(
        q(&s, &[b"OFFSET", b"4", b"LIMIT", b"2"]),
        flat_reply(&[("v:5", "50"), ("v:6", "60")])
    );
    assert_eq!(q(&s, &[b"OFFSET", b"100"]), flat_reply(&[]), "past the end = empty, not error");
}

#[test]
fn cursor_refuses_the_selection_clauses_by_name() {
    let s = seeded();
    for tail in [
        &[b"CURSOR" as &[u8], b"0", b"SORT", b"city", b"ASC"][..],
        &[b"CURSOR", b"0", b"DISTINCT", b"city"],
        &[b"CURSOR", b"0", b"FACET", b"city"],
        &[b"CURSOR", b"0", b"OFFSET", b"1"],
    ] {
        assert_eq!(
            q(&s, tail),
            b"-ERR IDX.QUERY 'vals': CURSOR cannot combine with SORT|DISTINCT|FACET|OFFSET\r\n"
                .to_vec()
        );
    }
}

#[test]
fn clause_errors_name_the_field_and_the_type() {
    let s = seeded();
    assert_eq!(
        String::from_utf8_lossy(&q(&s, &[b"FILTER", b"nope", b"EQ", b"1"])),
        "-ERR IDX.QUERY 'vals': FILTER names field 'nope', which this index does not store — it stores: city, price\r\n"
    );
    assert_eq!(
        String::from_utf8_lossy(&q(&s, &[b"FILTER", b"price", b"EQ", b"abc"])),
        "-ERR IDX.QUERY 'vals': FILTER bound 'abc' is not a valid i64, which is how this index declares 'price'\r\n"
    );
    assert_eq!(
        String::from_utf8_lossy(&q(&s, &[b"SORT", b"nope", b"ASC"])),
        "-ERR IDX.QUERY 'vals': SORT names field 'nope', which this index does not store — it stores: city, price\r\n"
    );
}

#[test]
fn values_update_and_delete_track_the_row() {
    let s = seeded();
    run(&s, &[b"HSET", b"v:3", b"city", b"tokyo"]);
    let r = q(&s, &[b"FILTER", b"city", b"EQ", b"tokyo"]);
    assert_eq!(r, flat_reply(&[("v:1", "10"), ("v:3", "30"), ("v:4", "40")]));
    run(&s, &[b"DEL", b"v:1"]);
    let r = q(&s, &[b"FILTER", b"city", b"EQ", b"tokyo"]);
    assert_eq!(r, flat_reply(&[("v:3", "30"), ("v:4", "40")]));
}

#[test]
fn values_on_unique_kind_and_refused_on_agg() {
    let s = store();
    run(&s, &[b"HSET", b"u:1", b"sku", b"a1", b"city", b"tokyo"]);
    run(&s, &[b"HSET", b"u:2", b"sku", b"b2", b"city", b"osaka"]);
    let r = run(
        &s,
        &[
            b"IDX.CREATE",
            b"uniq",
            b"ON",
            b"PREFIX",
            b"u:",
            b"FIELD",
            b"sku",
            b"TYPE",
            b"str",
            b"KIND",
            b"unique",
            b"VALUES",
            b"city",
        ],
    );
    assert_eq!(r, b"+OK\r\n");
    let r = run(&s, &[b"IDX.QUERY", b"uniq", b"EQ", b"a1", b"FILTER", b"city", b"EQ", b"tokyo"]);
    assert_eq!(r, flat_reply(&[("u:1", "a1")]));
    let r = run(
        &s,
        &[
            b"IDX.CREATE",
            b"agg",
            b"ON",
            b"PREFIX",
            b"u:",
            b"FIELD",
            b"n",
            b"TYPE",
            b"i64",
            b"KIND",
            b"agg",
            b"GROUPBY",
            b"city",
            b"VALUES",
            b"city",
        ],
    );
    assert_eq!(r, b"-ERR VALUES requires KIND text|range|unique\r\n".to_vec());
}

#[test]
fn scalar_values_survive_a_restart_via_the_sidecar() {
    let dir = kevy_tmpdir::TmpDir::new("scalar-values-sidecar");
    let cfg = || Config::default().with_ttl_reaper_manual().with_persist(dir.path());
    {
        let s = Store::open(cfg()).expect("open");
        run(&s, &[b"HSET", b"v:1", b"age", b"10", b"city", b"tokyo"]);
        run(&s, &[b"HSET", b"v:2", b"age", b"20", b"city", b"osaka"]);
        let r = run(
            &s,
            &[
                b"IDX.CREATE",
                b"vals",
                b"ON",
                b"PREFIX",
                b"v:",
                b"FIELD",
                b"age",
                b"TYPE",
                b"i64",
                b"KIND",
                b"range",
                b"VALUES",
                b"city",
            ],
        );
        assert_eq!(r, b"+OK\r\n");
    }
    let s = Store::open(cfg()).expect("reopen");
    let r = run(
        &s,
        &[b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100", b"FILTER", b"city", b"EQ", b"tokyo"],
    );
    assert_eq!(r, flat_reply(&[("v:1", "10")]), "the VALUES declaration survived the restart");
}
