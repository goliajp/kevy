//! Inline-flag and class tests for the vendored engine.
//!
//! Two provenances, kept apart on purpose because `src/tests.rs` states
//! the rule for this crate — nothing invented — and only half of this file
//! can claim it.
//!
//! **Ported.** The `(?x)` and `(?i)`/`(?c)` cases come from spg's e2e
//! corpus (`e2e_regex_extended_flag.rs`, `e2e_regex_are_round223.rs`),
//! both headed "Oracle values from PG 18.4". spg asserts them at the SQL
//! surface; the claims are about the PATTERN, so they are asserted here at
//! the engine, where they hold verbatim rather than through a translation
//! of output formatting.
//!
//! **Written here.** The POSIX bracket-class and `{m,n}` cases are not
//! transcribed from anything — they are written against POSIX ERE's
//! specified semantics, because no oracle for them was available in either
//! corpus. They are weaker evidence than the ported half and are marked as
//! such where they sit. If a PG oracle for these ever lands in the
//! funcgate corpus, they should be replaced by it rather than kept
//! alongside.
//!
//! These target the paths the dead-path atlas found unexercised in this
//! fork: `parse::fold_case` (62 never-executed regions),
//! `parse::strip_regex_extended_whitespace` (51) and
//! `classes::posix_class_members` (63). The engine arrived byte-identical
//! from spg; its proofs did not arrive with it.

#![cfg(test)]

use crate::regex_engine::{re_compile, re_find};

fn chars(s: &str) -> Vec<char> {
    s.chars().collect()
}

/// Does `pat` match anywhere in `hay`, and where?
fn find(pat: &str, hay: &str) -> Option<(usize, usize)> {
    let node = re_compile(pat).unwrap_or_else(|_| panic!("{pat} must compile"));
    re_find(&node, &chars(hay), 0).unwrap_or_else(|_| panic!("{pat} must not error"))
}

fn matched(pat: &str, hay: &str) -> Option<String> {
    find(pat, hay).map(|(s, e)| chars(hay)[s..e].iter().collect())
}

// spg e2e_regex_extended_flag: unescaped whitespace outside a character
// class is ignored under `(?x)`.
#[test]
fn extended_flag_ignores_unescaped_whitespace() {
    assert_eq!(matched("(?x) a b c", "abc").as_deref(), Some("abc"));
}

// spg e2e_regex_extended_flag: `#` starts an end-of-line comment.
#[test]
fn extended_flag_strips_hash_comments() {
    assert_eq!(
        matched(r"(?x) \d+  # the digits", "foo123bar").as_deref(),
        Some("123")
    );
}

// spg e2e_regex_extended_flag: an escaped space and a space inside a class
// both survive extended mode.
#[test]
fn extended_flag_keeps_escaped_and_class_whitespace() {
    assert_eq!(matched(r"(?x)a\ b", "a b").as_deref(), Some("a b"));
    assert_eq!(matched("(?x)[ ]", "a b").as_deref(), Some(" "));
}

// spg e2e_regex_extended_flag: without the flag, whitespace is literal —
// the control that makes the three above mean something.
#[test]
fn without_extended_flag_whitespace_is_literal() {
    assert_eq!(matched("a b c", "a b c").as_deref(), Some("a b c"));
    assert_eq!(matched("a b c", "abc"), None);
}

// spg e2e_regex_are_round223: embedded options.
#[test]
fn embedded_case_options_match_pg() {
    assert!(find("(?i)abc", "ABC").is_some(), "(?i) folds case");
    assert!(find("(?c)abc", "abc").is_some(), "(?c) is case-sensitive");
    assert!(find("(?c)abc", "ABC").is_none(), "(?c) does not fold");
    assert!(find("abc", "ABC").is_none(), "no flag does not fold either");
}

// WRITTEN HERE, not transcribed: POSIX ERE's specified class semantics.
// `classes::posix_class_members` is the largest unexercised block outside
// the capture matcher, and no oracle for it existed in either corpus.
#[test]
fn posix_bracket_classes_select_their_members() {
    assert_eq!(matched("[[:digit:]]+", "ab123cd").as_deref(), Some("123"));
    assert_eq!(matched("[[:alpha:]]+", "12abc34").as_deref(), Some("abc"));
    assert_eq!(matched("[[:space:]]", "a\tb").as_deref(), Some("\t"));
    assert_eq!(matched("[[:upper:]]+", "abCDef").as_deref(), Some("CD"));
    assert_eq!(matched("[[:punct:]]", "ab,cd").as_deref(), Some(","));
    assert_eq!(matched("[[:alnum:]]+", "  a1b2  ").as_deref(), Some("a1b2"));
    assert_eq!(matched("[[:xdigit:]]+", "zzBEEFzz").as_deref(), Some("BEEF"));
    // Negated, and combined with ordinary members.
    assert_eq!(matched("[^[:digit:]]+", "12ab34").as_deref(), Some("ab"));
    assert_eq!(matched("[[:digit:]x]+", "aax9x").as_deref(), Some("x9x"));
}

// WRITTEN HERE, not transcribed: POSIX ERE's specified bound semantics,
// exercising `parse_class::re_parse_bound`.
#[test]
fn counted_repetition_bounds() {
    assert_eq!(matched("a{2,3}", "aaaa").as_deref(), Some("aaa"));
    assert_eq!(matched("a{2}", "aaaa").as_deref(), Some("aa"));
    assert_eq!(matched("a{2,}", "aaaa").as_deref(), Some("aaaa"));
    assert_eq!(matched("^a{3}$", "aa"), None);
}
