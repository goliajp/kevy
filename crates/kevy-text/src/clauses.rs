//! Query-clause parsing: turning a raw query string into bare terms,
//! quoted phrases and `word*` prefixes.
//!
//! Split out of `segment_phrase` in v6. That file had reached exactly the
//! 500-line ceiling, and the two halves were never one job: everything
//! above it matches phrases against a segment's stored positions, while
//! this half never touches a segment at all — it reads bytes and produces
//! clauses, and its only dependency is the tokenizer.

use crate::token::tokenize;

/// Parsed query clauses: bare terms, phrases (each a token sequence) and
/// prefix stems.
pub type Clauses = (Vec<Vec<u8>>, Vec<Vec<Vec<u8>>>, Vec<Vec<u8>>);

/// Split a query into bare terms, quoted phrases, and `word*` prefixes.
/// A `"…"` group of two or more tokens is a phrase (a shorter group joins
/// the bare terms — a one-word "phrase" is just that word); an unquoted
/// word ending in `*` is a prefix. An unterminated quote is lenient: the
/// remainder is read as plain text rather than rejected.
///
/// # Examples
///
/// The three clause kinds come back separated: bare terms, quoted phrases,
/// and `*`-suffixed prefixes.
///
/// ```
/// let (terms, phrases, prefixes) =
///     kevy_text::parse_clauses(br#"alpha "two words" beta*"#);
/// assert_eq!(terms, vec![b"alpha".to_vec()]);
/// assert_eq!(phrases, vec![vec![b"two".to_vec(), b"words".to_vec()]]);
/// assert_eq!(prefixes, vec![b"beta".to_vec()]);
/// ```
///
/// A one-word "phrase" is not a phrase — it joins the bare terms, because a
/// phrase of one word is that word.
///
/// ```
/// let (terms, phrases, _) = kevy_text::parse_clauses(br#""solo""#);
/// assert_eq!(terms, vec![b"solo".to_vec()]);
/// assert!(phrases.is_empty());
/// ```
///
/// An unterminated quote is read as plain text rather than refused.
///
/// ```
/// let (terms, phrases, _) = kevy_text::parse_clauses(br#"open "never closed"#);
/// assert!(phrases.is_empty());
/// assert!(terms.contains(&b"open".to_vec()));
/// ```
pub fn parse_clauses(text: &[u8]) -> Clauses {
    let mut bare: Vec<Vec<u8>> = Vec::new();
    let mut phrases: Vec<Vec<Vec<u8>>> = Vec::new();
    let mut prefixes: Vec<Vec<u8>> = Vec::new();
    let mut plain: Vec<u8> = Vec::new();
    let mut i = 0;
    while i < text.len() {
        if text[i] != b'"' {
            plain.push(text[i]);
            i += 1;
            continue;
        }
        extend_plain(&plain, &mut bare, &mut prefixes);
        plain.clear();
        let start = i + 1;
        match text[start..].iter().position(|&b| b == b'"') {
            Some(off) => {
                let toks = tokenize(&text[start..start + off]);
                if toks.len() >= 2 {
                    phrases.push(toks);
                } else {
                    bare.extend(toks);
                }
                i = start + off + 1;
            }
            None => {
                extend_plain(&text[start..], &mut bare, &mut prefixes);
                i = text.len();
            }
        }
    }
    extend_plain(&plain, &mut bare, &mut prefixes);
    (bare, phrases, prefixes)
}

/// Split plain (unquoted) query text: a whitespace word ending in `*`
/// becomes a prefix clause (its stem, ASCII-lowercased to match the
/// stored token form), every other word tokenizes into bare terms.
fn extend_plain(plain: &[u8], bare: &mut Vec<Vec<u8>>, prefixes: &mut Vec<Vec<u8>>) {
    for word in plain.split(u8::is_ascii_whitespace) {
        match word.strip_suffix(b"*") {
            Some(stem) if !stem.is_empty() => {
                prefixes.push(stem.iter().map(u8::to_ascii_lowercase).collect());
            }
            _ => bare.extend(tokenize(word)),
        }
    }
}
