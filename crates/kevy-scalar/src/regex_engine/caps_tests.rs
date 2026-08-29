//! Capture-group tests for the vendored engine.
//!
//! `caps.rs` carried 438 never-executed regions — the largest single block
//! in this crate and, until now, the least defensible: kevy reaches it on
//! every `regexp_matches` and every `regexp_replace` with a `\1`, so this
//! is not spare capacity carried for fork fidelity. It is the live path.
//!
//! Provenance, split the way `flags_tests.rs` splits it:
//!
//! **Ported.** The backreference cases come from spg's
//! `e2e_regex_are_round223.rs` ("Oracle values from PG 18.4") — `'abcabc'
//! ~ '(abc)\1'` true, `'abcdef' ~ '(abc)\1'` false — asserted here at the
//! engine, where the claim is about the pattern rather than about SQL
//! output formatting.
//!
//! **Written here.** The group-numbering, non-participating-group and
//! nesting cases are written against POSIX ERE's specified semantics; no
//! oracle for them existed in either corpus. Weaker evidence than the
//! ported half, and marked so.

#![cfg(test)]

use crate::regex_engine::{max_group, re_compile, re_find_caps};

fn chars(s: &str) -> Vec<char> {
    s.chars().collect()
}

/// The whole match and each group, as text. `None` = the group did not
/// participate, which POSIX distinguishes from an empty match.
fn caps(pat: &str, hay: &str) -> Option<(String, Vec<Option<String>>)> {
    let node = re_compile(pat).unwrap_or_else(|_| panic!("{pat} must compile"));
    let n = max_group(&node);
    let cs = chars(hay);
    let m = re_find_caps(&node, &cs, 0, n).unwrap_or_else(|_| panic!("{pat} must not error"))?;
    let ((s, e), groups) = m;
    let txt = |sp: &Option<(usize, usize)>| sp.map(|(a, b)| cs[a..b].iter().collect::<String>());
    Some((cs[s..e].iter().collect(), groups[1..].iter().map(txt).collect()))
}

// WRITTEN HERE: group numbering follows opening-paren order.
#[test]
fn groups_are_numbered_by_opening_paren() {
    let (whole, g) = caps(r"(\d+)-(\d+)", "id 12-345 end").expect("matches");
    assert_eq!(whole, "12-345");
    assert_eq!(g, vec![Some("12".into()), Some("345".into())]);
}

// WRITTEN HERE: a nested group is numbered by its own opening paren, not
// by nesting depth.
#[test]
fn nested_groups_number_outer_first() {
    let (whole, g) = caps(r"((a+)(b+))c", "xaabbc").expect("matches");
    assert_eq!(whole, "aabbc");
    assert_eq!(
        g,
        vec![Some("aabb".into()), Some("aa".into()), Some("bb".into())]
    );
}

// WRITTEN HERE: a group on the losing side of an alternation did not
// participate, which is not the same as matching empty.
#[test]
fn non_participating_group_is_none_not_empty() {
    let (whole, g) = caps(r"(a)|(b)", "b").expect("matches");
    assert_eq!(whole, "b");
    assert_eq!(g, vec![None, Some("b".into())]);
}

// WRITTEN HERE: a group inside a repetition reports its LAST iteration.
#[test]
fn repeated_group_reports_its_last_iteration() {
    let (whole, g) = caps(r"(?:(a|b))+", "abab").expect("matches");
    assert_eq!(whole, "abab");
    assert_eq!(g, vec![Some("b".into())]);
}

// PORTED from spg e2e_regex_are_round223 (PG 18.4 oracle): a
// backreference matches what its group captured, and nothing else.
#[test]
fn backreference_matches_what_the_group_captured() {
    assert!(caps(r"(abc)\1", "abcabc").is_some(), "'abcabc' ~ '(abc)\\1'");
    assert!(caps(r"(abc)\1", "abcdef").is_none(), "'abcdef' ~ '(abc)\\1'");
}

// OBSERVED, NOT SOURCED. This started as a written-here assertion that a
// backreference to a non-participating group cannot match; the engine
// disagreed, and there is no oracle for it in either corpus. What it does
// is treat such a backreference as matching the empty string:
//
//     ^(?:(a)|b)\1$  vs "b"   -> matches (0,1)
//     (a)?b\1        vs "b"   -> matches (0,1)
//
// PostgreSQL may well differ, and if it does, so does spg — the engine is
// byte-identical there. Pinned here as a regression guard on the CURRENT
// behaviour rather than as a claim about the correct one; see
// bench/FINDING-2026-08-28-a-backreference-to-an-unset-group.md. Replace
// with a PG oracle when one exists, in whichever direction it points.
#[test]
fn backreference_to_unmatched_group_matches_empty() {
    assert!(
        caps(r"^(?:(a)|b)\1$", "b").is_some(),
        "observed: an unset backreference matches empty"
    );
    assert!(caps(r"^(?:(a)|b)\1$", "aa").is_some(), "the set case matches");
    assert!(
        caps(r"^(?:(a)|b)\1$", "ba").is_none(),
        "and it is a real backreference, not a wildcard"
    );
}

// WRITTEN HERE: searching from a later position still numbers groups the
// same way — the capture matcher is position-independent.
#[test]
fn captures_hold_when_the_match_starts_late() {
    let (whole, g) = caps(r"(x)(y)", "aaaxy").expect("matches");
    assert_eq!(whole, "xy");
    assert_eq!(g, vec![Some("x".into()), Some("y".into())]);
}
