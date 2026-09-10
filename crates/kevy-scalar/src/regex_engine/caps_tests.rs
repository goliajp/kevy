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
    assert_eq!(g, vec![Some("aabb".into()), Some("aa".into()), Some("bb".into())]);
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
// behaviour rather than as a claim about the correct one. Replace
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

// WRITTEN HERE: a quantified capture group that a backreference then
// constrains. This is the arm `caps.rs` documents with
// `^(a*)\1$` on "aaaa" giving back group = "aa", and 138 of its regions
// had never executed — the largest surviving block in this crate.
//
// The arm exists because a greedy `(a*)` would take all four `a`s and
// leave nothing for `\1`. It has to enumerate the inner quantifier's
// reachable ends, record `caps[idx]` at each repetition count, and try the
// tail from each — a backtrack point the plain descent never needs,
// because it has no captures for a backreference to refer to.
#[test]
fn a_quantified_group_backtracks_for_the_backreference_that_follows_it() {
    // The documented case: greedy `(a*)` must give back to two.
    let (whole, g) = caps(r"^(a*)\1$", "aaaa").expect("matches");
    assert_eq!(whole, "aaaa");
    assert_eq!(g, vec![Some("aa".into())], "the group gives back half");

    // Odd length cannot split in two, at any give-back.
    assert!(caps(r"^(a*)\1$", "aaa").is_none(), "three a's cannot be a doubled prefix");
    assert!(caps(r"^(a*)\1$", "aaaaa").is_none());

    // Zero is a reachable end: the empty group matches the empty string.
    let (whole, g) = caps(r"^(a*)\1$", "").expect("empty matches with an empty group");
    assert_eq!(whole, "");
    assert_eq!(g, vec![Some("".into())]);

    // Three copies: eight a's give back to two, not four.
    let (whole, g) = caps(r"^(a*)\1\1$", "aaaaaa").expect("matches");
    assert_eq!(whole, "aaaaaa");
    assert_eq!(g, vec![Some("aa".into())], "six a's split three ways");

    // `+` has a floor of one, so the empty end is not reachable.
    assert!(caps(r"^(a+)\1$", "").is_none(), "a+ cannot match empty");
    let (_, g) = caps(r"^(a+)\1$", "aaaa").expect("matches");
    assert_eq!(g, vec![Some("aa".into())]);

    // A multi-character body, so the enumeration is over repetitions of a
    // group rather than of a single character.
    let (whole, g) = caps(r"^(ab)+\1$", "ababab").expect("matches");
    assert_eq!(whole, "ababab");
    assert_eq!(g, vec![Some("ab".into())], "the last repetition is what \\1 sees");

    // A bounded quantifier: the enumeration must stop at the ceiling.
    let (_, g) = caps(r"^(a{1,2})\1$", "aaaa").expect("matches");
    assert_eq!(g, vec![Some("aa".into())]);
    assert!(caps(r"^(a{1,2})\1$", "aaaaaa").is_none(), "the ceiling of two is enforced");
}

/// The two descents agree — at the low level where they are the same
/// algorithm, and at the high level where one of them routes.
///
/// `re_match_at` and `re_match_at_caps` are one backtracking descent
/// written twice, the second threading a `Caps` array and an undo journal
/// through it so a failed branch restores what it overwrote. Between them
/// they carry 123 never-executed regions, and they are what every
/// `regexp_matches` and every `regexp_replace` with a `\1` runs. Nothing
/// checked that they answer the same question the same way. Two
/// implementations of one descent is the shape
/// `mod/no-second-implementation` warns about: they drift, disagree on a
/// quantifier or a backtrack, and both stay green because each is tested
/// alone.
///
/// **Backreferences are not one of those disagreements**, and finding that
/// out is why this test has two halves. `re_match_at` answers `Ok(None)`
/// for any `Backref` — it has no captures to compare against — so
/// `(abc)\1` on `"abcabc"` returns no match there while the caps descent
/// returns 6. That is not drift: `re_find` checks `has_backref` and routes
/// those patterns to the capturing side, discarding the captures. The
/// low-level half therefore compares only patterns without backreferences,
/// which is the domain where the two are meant to be interchangeable, and
/// the high-level half compares `re_find` against `re_find_caps` over
/// everything — which is where the routing itself gets checked.
///
/// One difference is deliberate and is why this stays shallow:
/// `MATCH_DEPTH_LIMIT` is 500 and `CAP_MATCH_DEPTH_LIMIT` is 300, so a
/// pattern nested between those errors in one and matches in the other.
#[test]
fn the_capturing_and_non_capturing_descents_agree() {
    use crate::regex_engine::{has_backref, re_find, re_find_caps, re_match_at, re_match_at_caps};

    // Quantifiers, alternation, classes, anchors, backtracking, and the
    // empty-match cases that separate a correct descent from a plausible one.
    let patterns = [
        "a*b",
        "a+b",
        "a?b",
        "(a|b)+c",
        "(a|ab)c",
        "[a-z]{2,4}",
        "^ab$",
        "a{3}",
        "[^x]+",
        "(ab)*",
        "(a*)*b",
        "x|",
        "()",
        "a**",
        "(a|b|c)d",
        "[[:digit:]]+",
        "a.c",
        "(abc)\\1",
        "^$",
        "(a)(b)(c)",
    ];
    let inputs = [
        "", "a", "b", "ab", "abc", "aab", "abab", "abcabc", "xyz", "aaaa", "a1b2", "ac", "d", "cd",
        "123", "  ", "aXc",
    ];

    let (mut low, mut high, mut matched, mut backref_pats) = (0, 0, 0, 0);
    for pat in patterns {
        // A pattern this engine refuses to compile is not a disagreement
        // between the descents, which is what this test is about.
        let Ok(node) = re_compile(pat) else { continue };
        let ngroups = max_group(&node);
        let routed = has_backref(&node);
        if routed {
            backref_pats += 1;
        }
        for input in inputs {
            let s = chars(input);

            // High level: both entries, every pattern, routing included.
            let plain_span = re_find(&node, &s, 0);
            let caps_span = re_find_caps(&node, &s, 0, ngroups).map(|o| o.map(|(span, _)| span));
            match (&plain_span, &caps_span) {
                (Ok(a), Ok(b)) => assert_eq!(
                    a, b,
                    "pattern {pat:?} on {input:?}: re_find says {a:?}, re_find_caps says {b:?}"
                ),
                (Err(_), Err(_)) => {}
                _ => {
                    panic!("pattern {pat:?} on {input:?}: one entry errored and the other did not")
                }
            }
            high += 1;

            // Low level: only where the two are meant to be interchangeable.
            if routed {
                continue;
            }
            let mut steps_plain: u64 = 0;
            let plain = re_match_at(&node, &s, 0, 0, &mut steps_plain);
            let mut steps_caps: u64 = 0;
            let mut caps: Vec<Option<(usize, usize)>> = vec![None; ngroups + 1];
            let mut journal: Vec<(usize, Option<(usize, usize)>)> = Vec::new();
            let capped =
                re_match_at_caps(&node, &s, 0, 0, &mut steps_caps, &mut caps, &mut journal);
            match (&plain, &capped) {
                (Ok(a), Ok(b)) => {
                    assert_eq!(
                        a, b,
                        "pattern {pat:?} on {input:?}: re_match_at says {a:?}, \
                         re_match_at_caps says {b:?}"
                    );
                    if a.is_some() {
                        matched += 1;
                    }
                }
                (Err(_), Err(_)) => {}
                _ => panic!(
                    "pattern {pat:?} on {input:?}: one matcher errored and the other did not"
                ),
            }
            low += 1;
        }
    }

    assert_eq!(high, patterns.len() * inputs.len(), "every pair went through both entries");
    assert!(
        backref_pats > 0,
        "the table must contain a backref pattern, or the routing half
             of this test compares nothing"
    );
    assert!(low > 0 && low < high, "the low-level half must run and must skip the routed ones");
    assert!(
        matched > 40,
        "only {matched} of {low} low-level pairs matched; a table where almost nothing \
         matches compares two ways of saying no"
    );
}
