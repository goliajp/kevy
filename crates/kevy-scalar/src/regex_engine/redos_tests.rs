//! ReDoS-safety tests, ported with the engine they test.
//!
//! `regex_engine` is a byte-identical fork of spg's ERE matcher, and the
//! fork arrived with its four safety caps — `PARSE_DEPTH_LIMIT`,
//! `MATCH_DEPTH_LIMIT`, `REPEAT_MAX`, `MATCH_STEP_LIMIT` — but without the
//! tests that prove they fire. All four are identical on both sides
//! (verified, 2026-08-27), so the tests port without adaptation beyond
//! module paths and the std/alloc split.
//!
//! Source: `spg/crates/spg-engine/src/eval/regexp.rs`, `mod redos_tests`.
//! Nothing here is invented, which is the same rule `src/tests.rs` states
//! for the pg_regress transcriptions — a forked engine's proofs come with
//! the fork.
//!
//! These cover the limit paths the dead-path atlas found unexercised:
//! parser recursion, matcher recursion, bound parsing and step budget.

#![cfg(test)]

mod redos_tests {
    use std::thread;

    use crate::regex_engine::{re_compile, re_find};

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    fn repeat_char(c: char, n: usize) -> String {
        core::iter::repeat_n(c, n).collect()
    }

    fn repeat_char_str(s: &str, n: usize) -> String {
        core::iter::repeat_n(s, n).collect()
    }

    // (a) A deeply-nested pattern hits the parser recursion cap and
    // returns a clean Err instead of overflowing the parser stack.
    #[test]
    fn redos_deep_nested_groups_parse_error() {
        let pat = repeat_char('(', 5000); // 5000 unbalanced groups
        let res = re_compile(&pat);
        assert!(res.is_err(), "deeply-nested groups must be a clean error");
    }

    // (a) A pattern that drives the *matcher* recursion past its cap
    // returns a clean Err — proven not to overflow even on a 1 MiB
    // stack (smaller than tokio's 2 MiB / pthread's 8 MiB defaults).
    #[test]
    fn redos_deep_match_returns_err_not_overflow() {
        let handle = thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(|| {
                // Flat literal concat far deeper than MATCH_DEPTH_LIMIT;
                // matching it against an equally long haystack recurses
                // once per element.
                let pat = repeat_char('a', 6000);
                let node = re_compile(&pat).expect("flat literal compiles");
                let hay = chars(&repeat_char('a', 6000));
                re_find(&node, &hay, 0)
            })
            .expect("spawn");
        let res = handle.join().expect("match thread must not overflow/panic");
        assert!(
            res.is_err(),
            "over-deep match must abort with a clean error"
        );
    }

    // (b) An `{m,n}` bound with n > REPEAT_MAX (65535) is rejected as
    // an invalid regex at parse time.
    #[test]
    fn redos_repeat_bound_over_cap_rejected() {
        assert!(re_compile("a{0,70000}").is_err());
        assert!(re_compile("a{70000}").is_err());
        assert!(re_compile("a{999999999999999999999}").is_err());
        // n < m is likewise an invalid regex.
        assert!(re_compile("a{5,2}").is_err());
        // At the ceiling is still accepted.
        assert!(re_compile("a{0,65535}").is_ok());
    }

    // (c) A normal counted-repetition pattern still compiles and
    // matches correctly (real quantifier semantics, not literal text).
    #[test]
    fn redos_normal_bounds_match_correctly() {
        let node = re_compile("^a{1,5}$").expect("compiles");
        // 3 and 5 a's match; 0 and 6 do not.
        assert_eq!(
            re_find(&node, &chars("aaa"), 0).unwrap(),
            Some((0, 3))
        );
        assert_eq!(
            re_find(&node, &chars("aaaaa"), 0).unwrap(),
            Some((0, 5))
        );
        assert_eq!(re_find(&node, &chars(""), 0).unwrap(), None);
        assert_eq!(re_find(&node, &chars("aaaaaa"), 0).unwrap(), None);

        // `{m}` exact and `{m,}` open bounds.
        let exact = re_compile("^a{3}$").expect("compiles");
        assert!(re_find(&exact, &chars("aaa"), 0).unwrap().is_some());
        assert!(re_find(&exact, &chars("aa"), 0).unwrap().is_none());
        let openb = re_compile("^a{2,}$").expect("compiles");
        assert!(re_find(&openb, &chars("aaaa"), 0).unwrap().is_some());
        assert!(re_find(&openb, &chars("a"), 0).unwrap().is_none());
    }

    // A stray `{` that does not form a valid bound stays a literal
    // brace — pre-existing behavior for patterns using `{` literally
    // must not change.
    #[test]
    fn redos_stray_brace_is_literal() {
        let node = re_compile("a{foo").expect("stray brace compiles as literal");
        assert_eq!(
            re_find(&node, &chars("a{foo"), 0).unwrap(),
            Some((0, 5))
        );
    }

    // A legitimately (but not pathologically) nested pattern still
    // compiles — the parser cap is far above real nesting.
    #[test]
    fn redos_moderate_nesting_ok() {
        let pat = format!("{}a{}", repeat_char('(', 50), repeat_char(')', 50));
        assert!(re_compile(&pat).is_ok());
    }

    // (a — TIME bound) A catastrophic-backtracking pattern on a long
    // non-matching input recurses only shallowly (so the depth cap never
    // fires) but explores super-linearly many paths. The total-step
    // budget must abort it with a clean Err *fast* — if this test ever
    // hangs, the step counter is not wired into the hot backtracking
    // loop.
    //
    // NB: this matcher matches a *standalone* quantifier greedily without
    // backtracking, so the textbook nested-quantifier bomb `(a+)+$` is
    // already defused (the inner `a+` grabs the whole run at once). The
    // residual hazard is *sequential* quantifiers: `a*a*…a*b` on an all-
    // `a` string with no `b` forces the seq matcher to enumerate every
    // non-decreasing split of the run across the k stars — C(N+k, k)
    // combinations, ~7.5e10 here — before it can conclude "no match".
    // That is unbounded CPU without a work budget.
    #[test]
    fn redos_catastrophic_backtracking_returns_err_fast() {
        use std::time::Instant;
        // 10 sequential `a*` then a literal `b`; input is 50 `a`s and no
        // `b`, so every combination must be tried and all fail.
        let pat = format!("{}b", repeat_char_str("a*", 10));
        let node = re_compile(&pat).expect("compiles");
        let hay = chars(&repeat_char('a', 50));
        let t0 = Instant::now();
        let res = re_find(&node, &hay, 0);
        let elapsed = t0.elapsed();
        assert!(
            res.is_err(),
            "catastrophic backtracking must abort with a clean budget error, got {:?}",
            res
        );
        // The budget aborts at MATCH_STEP_LIMIT (10M) steps regardless of
        // how large the combinatorial space is, so this is well under a
        // second; a generous ceiling guards against a wiring regression
        // (an unbounded matcher would run for many minutes / never).
        assert!(
            elapsed.as_secs() < 10,
            "budget-exceeded abort must be fast, took {:?}",
            elapsed
        );
    }

    // (b) A long but non-pathological input matches correctly — a linear
    // pattern spends only O(input) steps, orders of magnitude under the
    // step budget, so the budget never trips and results are unchanged.
    #[test]
    fn redos_normal_long_input_still_matches() {
        // `^a+b$` on 10000 'a's + 'b' matches (greedy '+' then one 'b').
        let node = re_compile("^a+b$").expect("compiles");
        let hay = chars(&format!("{}b", repeat_char('a', 10_000)));
        assert_eq!(
            re_find(&node, &hay, 0).unwrap(),
            Some((0, 10_001)),
            "a legitimate long match must succeed — budget must not trip"
        );
        // Same pattern, no trailing 'b' → clean non-match (not an error).
        let miss = chars(&repeat_char('a', 10_000));
        assert_eq!(re_find(&node, &miss, 0).unwrap(), None);
    }
}
