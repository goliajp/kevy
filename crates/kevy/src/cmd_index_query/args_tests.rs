//! Terminal MATCH surface parse tests, split from `args.rs` for the
//! 500-LOC house rule (a `#[path]` child of `args`).

    use super::*;

    fn argv(parts: &[&str]) -> Vec<Vec<u8>> {
        parts.iter().map(|p| p.as_bytes().to_vec()).collect()
    }

    /// Every reserved clause must come back named. Silently ignoring one
    /// is the failure mode worth a test: a dropped FILTER returns
    /// unfiltered rows, which is a wrong answer wearing a success reply.
    #[test]
    fn reserved_clauses_are_refused_by_name() {
        // HIGHLIGHT, IN, FILTER and SORT are no longer here — they ship
        // now (see the clause tests below); the rest stay reserved.
        for clause in ["FACET", "DISTINCT"] {
            let a = argv(&["IDX.QUERY", "idx", "MATCH", "hello", clause, "x"]);
            match MatchArgs::parse_terminal(&a) {
                MatchParse::NotYet(c) => {
                    assert!(c.eq_ignore_ascii_case(clause.as_bytes()), "{clause}");
                }
                _ => panic!("{clause} should be reserved, not accepted or rejected as bad"),
            }
        }
    }

    /// HIGHLIGHT parses now: with fields it names them, with none it means
    /// every field, and it coexists with FIELDS in any order.
    #[test]
    fn highlight_parses() {
        let none = argv(&["IDX.QUERY", "idx", "MATCH", "hello"]);
        assert!(matches!(MatchArgs::parse_terminal(&none),
            MatchParse::Ok(q) if q.highlight.is_none()));

        let all = argv(&["IDX.QUERY", "idx", "MATCH", "hello", "HIGHLIGHT"]);
        assert!(matches!(MatchArgs::parse_terminal(&all),
            MatchParse::Ok(q) if q.highlight == Some(vec![])));

        let some = argv(&["IDX.QUERY", "idx", "MATCH", "hello", "HIGHLIGHT", "title", "body"]);
        assert!(matches!(MatchArgs::parse_terminal(&some),
            MatchParse::Ok(q) if q.highlight == Some(vec![b"title".to_vec(), b"body".to_vec()])));

        // FIELDS then HIGHLIGHT: each variadic clause stops at the other.
        let both = argv(&["IDX.QUERY", "idx", "MATCH", "hi", "FIELDS", "a", "HIGHLIGHT", "b"]);
        match MatchArgs::parse_terminal(&both) {
            MatchParse::Ok(q) => {
                assert_eq!(q.fields, vec![b"a".to_vec()]);
                assert_eq!(q.highlight, Some(vec![b"b".to_vec()]));
            }
            _ => panic!("FIELDS and HIGHLIGHT must coexist"),
        }
    }

    #[test]
    fn the_shipped_surface_still_parses() {
        let a = argv(&["IDX.QUERY", "idx", "MATCH", "hello", "LIMIT", "5", "FIELDS", "title"]);
        match MatchArgs::parse_terminal(&a) {
            MatchParse::Ok(q) => {
                assert_eq!(q.text, b"hello");
                assert_eq!(q.limit, 5);
                assert_eq!(q.fields, vec![b"title".to_vec()]);
            }
            _ => panic!("the existing surface must keep working unchanged"),
        }
    }

    #[test]
    fn genuine_nonsense_is_still_bad_args() {
        let a = argv(&["IDX.QUERY", "idx", "MATCH", "hello", "WOBBLE"]);
        assert!(matches!(MatchArgs::parse_terminal(&a), MatchParse::BadArgs));
    }

    /// A reserved word appearing as the search text is a query, not a
    /// clause -- the scan starts after the text argument.
    #[test]
    fn a_reserved_word_as_the_query_text_is_not_a_clause() {
        let a = argv(&["IDX.QUERY", "idx", "MATCH", "FILTER"]);
        assert!(matches!(MatchArgs::parse_terminal(&a), MatchParse::Ok(_)));
    }

/// TYPO ships now: 0/1/2 parse into a budget; anything else — including
/// the frozen surface's AUTO, which is not built — is a syntax error
/// rather than a silently clamped budget.
#[test]
fn typo_parses() {
    let none = argv(&["IDX.QUERY", "idx", "MATCH", "hello"]);
    assert!(matches!(MatchArgs::parse_terminal(&none), MatchParse::Ok(q) if q.typo == 0));
    for (v, want) in [("0", 0u32), ("1", 1), ("2", 2)] {
        let a = argv(&["IDX.QUERY", "idx", "MATCH", "hello", "TYPO", v]);
        assert!(
            matches!(MatchArgs::parse_terminal(&a), MatchParse::Ok(q) if q.typo == want),
            "TYPO {v}"
        );
    }
    for bad in ["AUTO", "3", "x"] {
        let a = argv(&["IDX.QUERY", "idx", "MATCH", "hello", "TYPO", bad]);
        assert!(matches!(MatchArgs::parse_terminal(&a), MatchParse::BadArgs), "TYPO {bad}");
    }
    // Coexists with the other clauses, in either order.
    let both = argv(&["IDX.QUERY", "idx", "MATCH", "hi", "TYPO", "1", "FIELDS", "a"]);
    match MatchArgs::parse_terminal(&both) {
        MatchParse::Ok(q) => {
            assert_eq!(q.typo, 1);
            assert_eq!(q.fields, vec![b"a".to_vec()]);
        }
        _ => panic!("TYPO and FIELDS must coexist"),
    }
}

/// OFFSET ships now: it parses into a skip count and coexists with the
/// other clauses.
#[test]
fn offset_parses() {
    let none = argv(&["IDX.QUERY", "idx", "MATCH", "hello"]);
    assert!(matches!(MatchArgs::parse_terminal(&none), MatchParse::Ok(q) if q.offset == 0));
    let a = argv(&["IDX.QUERY", "idx", "MATCH", "hello", "LIMIT", "5", "OFFSET", "10"]);
    match MatchArgs::parse_terminal(&a) {
        MatchParse::Ok(q) => {
            assert_eq!(q.limit, 5);
            assert_eq!(q.offset, 10);
        }
        _ => panic!("LIMIT and OFFSET must coexist"),
    }
    let bad = argv(&["IDX.QUERY", "idx", "MATCH", "hello", "OFFSET", "x"]);
    assert!(matches!(MatchArgs::parse_terminal(&bad), MatchParse::BadArgs));
}

/// IN ships now: it names the declared fields a query scores within, and
/// coexists with every other clause in any order.
#[test]
fn scope_parses() {
    let none = argv(&["IDX.QUERY", "idx", "MATCH", "hello"]);
    assert!(matches!(MatchArgs::parse_terminal(&none), MatchParse::Ok(q) if q.scope.is_empty()));

    let one = argv(&["IDX.QUERY", "idx", "MATCH", "hello", "IN", "title"]);
    match MatchArgs::parse_terminal(&one) {
        MatchParse::Ok(q) => assert_eq!(q.scope, vec![b"title".to_vec()]),
        _ => panic!("IN <field> should parse"),
    }

    // Variadic, and it stops at the next clause keyword rather than
    // swallowing it.
    let many =
        argv(&["IDX.QUERY", "idx", "MATCH", "hi", "IN", "title", "body", "LIMIT", "3", "TYPO", "1"]);
    match MatchArgs::parse_terminal(&many) {
        MatchParse::Ok(q) => {
            assert_eq!(q.scope, vec![b"title".to_vec(), b"body".to_vec()]);
            assert_eq!(q.limit, 3);
            assert_eq!(q.typo, 1);
        }
        _ => panic!("IN must stop at the next keyword"),
    }

    // An empty IN is a syntax error, not "every field" — the query said
    // it wanted a scope.
    let empty = argv(&["IDX.QUERY", "idx", "MATCH", "hi", "IN"]);
    assert!(matches!(MatchArgs::parse_terminal(&empty), MatchParse::BadArgs));
}

/// Pass 2 parses the clauses with the same code pass 1 does, so a clause
/// cannot mean one thing to the shard fan-out and another to the merge.
#[test]
fn pass_two_reparses_every_clause() {
    let argv2 = argv(&[
        "MATCH.SCORE", "idx", "hello", "LIMIT=7", "<gstats>", "FIELDS", "a", "HIGHLIGHT", "TYPO",
        "2", "OFFSET", "4", "IN", "title", "body",
    ]);
    let q = crate::cmd_index_query::parse_match_score(&argv2).expect("pass-2 argv parses");
    assert_eq!(q.name, b"idx".to_vec());
    assert_eq!(q.text, b"hello".to_vec());
    assert_eq!(q.limit, 7);
    assert_eq!(q.fields, vec![b"a".to_vec()]);
    assert_eq!(q.highlight, Some(Vec::new()), "bare HIGHLIGHT = every field");
    assert_eq!(q.typo, 2);
    assert_eq!(q.offset, 4);
    assert_eq!(q.scope, vec![b"title".to_vec(), b"body".to_vec()]);
}

/// FILTER ships now: it reuses the index query grammar's own RANGE / EQ
/// rather than a second expression language, and several AND together.
#[test]
fn filter_parses() {
    let none = argv(&["IDX.QUERY", "idx", "MATCH", "hello"]);
    assert!(matches!(MatchArgs::parse_terminal(&none), MatchParse::Ok(q) if q.filters.is_empty()));

    let one = argv(&["IDX.QUERY", "idx", "MATCH", "hi", "FILTER", "price", "RANGE", "10", "100"]);
    match MatchArgs::parse_terminal(&one) {
        MatchParse::Ok(q) => {
            assert_eq!(q.filters.len(), 1);
            assert_eq!(q.filters[0].field, b"price".to_vec());
            assert_eq!(
                q.filters[0].shape,
                FilterShape::Range { min: b"10".to_vec(), max: b"100".to_vec() }
            );
        }
        _ => panic!("FILTER … RANGE should parse"),
    }

    // Several predicates AND, and they coexist with the other clauses in
    // any order.
    let many = argv(&[
        "IDX.QUERY", "idx", "MATCH", "hi", "FILTER", "price", "RANGE", "1", "9", "IN", "title",
        "FILTER", "status", "EQ", "live", "LIMIT", "3",
    ]);
    match MatchArgs::parse_terminal(&many) {
        MatchParse::Ok(q) => {
            assert_eq!(q.filters.len(), 2, "both predicates kept");
            assert_eq!(q.filters[1].shape, FilterShape::Eq { value: b"live".to_vec() });
            assert_eq!(q.scope, vec![b"title".to_vec()]);
            assert_eq!(q.limit, 3);
        }
        _ => panic!("FILTER must compose with the other clauses"),
    }

    for bad in [
        vec!["IDX.QUERY", "idx", "MATCH", "hi", "FILTER"],
        vec!["IDX.QUERY", "idx", "MATCH", "hi", "FILTER", "price"],
        vec!["IDX.QUERY", "idx", "MATCH", "hi", "FILTER", "price", "NEAR", "5"],
        vec!["IDX.QUERY", "idx", "MATCH", "hi", "FILTER", "price", "RANGE", "10"],
    ] {
        let a = argv(&bad);
        assert!(matches!(MatchArgs::parse_terminal(&a), MatchParse::BadArgs), "{bad:?}");
    }
}


/// SORT ships now: it names a stored field and a direction, and it is
/// carried to pass 2 because that is where the selection happens.
#[test]
fn sort_parses() {
    let none = argv(&["IDX.QUERY", "idx", "MATCH", "hi"]);
    assert!(matches!(MatchArgs::parse_terminal(&none), MatchParse::Ok(q) if q.sort.is_none()));

    for (dir, desc) in [("ASC", false), ("DESC", true), ("desc", true)] {
        let a = argv(&["IDX.QUERY", "idx", "MATCH", "hi", "SORT", "price", dir]);
        match MatchArgs::parse_terminal(&a) {
            MatchParse::Ok(q) => assert_eq!(q.sort, Some((b"price".to_vec(), desc)), "{dir}"),
            _ => panic!("SORT {dir} should parse"),
        }
    }

    // Composes with the rest, and pass 2 re-parses it identically.
    let full = argv(&[
        "IDX.QUERY", "idx", "MATCH", "hi", "SORT", "price", "ASC", "FILTER", "price",
        "RANGE", "1", "9", "LIMIT", "5",
    ]);
    match MatchArgs::parse_terminal(&full) {
        MatchParse::Ok(q) => {
            assert_eq!(q.sort, Some((b"price".to_vec(), false)));
            assert_eq!(q.filters.len(), 1);
            assert_eq!(q.limit, 5);
        }
        _ => panic!("SORT must compose"),
    }

    // A direction is required and must be one of the two.
    for bad in [
        vec!["IDX.QUERY", "idx", "MATCH", "hi", "SORT"],
        vec!["IDX.QUERY", "idx", "MATCH", "hi", "SORT", "price"],
        vec!["IDX.QUERY", "idx", "MATCH", "hi", "SORT", "price", "SIDEWAYS"],
    ] {
        let a = argv(&bad);
        assert!(matches!(MatchArgs::parse_terminal(&a), MatchParse::BadArgs), "{bad:?}");
    }
}
