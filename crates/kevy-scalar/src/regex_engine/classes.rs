//! (vendored regex engine — see the parent module.)
#![allow(clippy::all, clippy::pedantic)]
use super::*;

pub(crate) fn class_matches(member: &ClassMember, c: char) -> bool {
    match member {
        ClassMember::Single(s) => *s == c,
        ClassMember::Range(a, b) => c >= *a && c <= *b,
        ClassMember::NotInSet(subs) => !subs.iter().any(|m| class_matches(m, c)),
    }
}

/// v7.37.16 regex slice — the ASCII member list of a `\d`/`\w`/`\s`
/// shortcut, shared by the top-level escape parser (`re_parse_atom`) and
/// the in-bracket parser (`re_parse_class`) so both stay byte-identical.
/// `\v`/`\f` are included in the space set to match PG's `[[:space:]]`.
pub(crate) fn shortcut_members(kind: char) -> Vec<ClassMember> {
    match kind {
        'd' | 'D' => vec![ClassMember::Range('0', '9')],
        'w' | 'W' => vec![
            ClassMember::Range('a', 'z'),
            ClassMember::Range('A', 'Z'),
            ClassMember::Range('0', '9'),
            ClassMember::Single('_'),
        ],
        's' | 'S' => vec![
            ClassMember::Single(' '),
            ClassMember::Single('\t'),
            ClassMember::Single('\n'),
            ClassMember::Single('\r'),
            ClassMember::Single('\u{0b}'), // vertical tab
            ClassMember::Single('\u{0c}'), // form feed
        ],
        _ => Vec::new(),
    }
}

/// v7.37.16 regex slice — the ASCII member list of a POSIX class name
/// (`alpha`, `digit`, …) as it appears inside `[[:name:]]`. Returns
/// `Err` for an unknown name, matching PG18's "invalid character class"
/// compile error. Scoped to ASCII, consistent with this engine's
/// ASCII-only `\w`/`\d` handling (the matcher does not decode UTF-8).
pub(crate) fn posix_class_members(name: &str) -> Result<Vec<ClassMember>, ReErr> {
    let members = match name {
        "alpha" => vec![ClassMember::Range('a', 'z'), ClassMember::Range('A', 'Z')],
        "digit" => vec![ClassMember::Range('0', '9')],
        "alnum" => vec![
            ClassMember::Range('a', 'z'),
            ClassMember::Range('A', 'Z'),
            ClassMember::Range('0', '9'),
        ],
        "upper" => vec![ClassMember::Range('A', 'Z')],
        "lower" => vec![ClassMember::Range('a', 'z')],
        "xdigit" => vec![
            ClassMember::Range('0', '9'),
            ClassMember::Range('a', 'f'),
            ClassMember::Range('A', 'F'),
        ],
        "word" => vec![
            ClassMember::Range('a', 'z'),
            ClassMember::Range('A', 'Z'),
            ClassMember::Range('0', '9'),
            ClassMember::Single('_'),
        ],
        "space" => vec![
            ClassMember::Single(' '),
            ClassMember::Single('\t'),
            ClassMember::Single('\n'),
            ClassMember::Single('\r'),
            ClassMember::Single('\u{0b}'),
            ClassMember::Single('\u{0c}'),
        ],
        "blank" => vec![ClassMember::Single(' '), ClassMember::Single('\t')],
        "cntrl" => vec![
            ClassMember::Range('\u{00}', '\u{1f}'),
            ClassMember::Single('\u{7f}'),
        ],
        "print" => vec![ClassMember::Range('\u{20}', '\u{7e}')],
        "graph" => vec![ClassMember::Range('\u{21}', '\u{7e}')],
        "punct" => vec![
            ClassMember::Range('\u{21}', '\u{2f}'),
            ClassMember::Range('\u{3a}', '\u{40}'),
            ClassMember::Range('\u{5b}', '\u{60}'),
            ClassMember::Range('\u{7b}', '\u{7e}'),
        ],
        _ => {
            return Err(ReErr::TypeMismatch {
                detail: "invalid regular expression: invalid character class".into(),
            });
        }
    };
    Ok(members)
}
