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
        // HIGHLIGHT is no longer here — it ships now (see the highlight
        // tests below); the rest stay reserved-by-name.
        for clause in ["IN", "FILTER", "FACET", "SORT", "DISTINCT", "TYPO", "OFFSET"] {
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
