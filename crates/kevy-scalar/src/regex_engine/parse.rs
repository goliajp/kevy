//! (vendored regex engine — see the parent module.)
#![allow(clippy::all, clippy::pedantic)]
use super::*;

pub(crate) fn fold_case(node: &mut ReNode) {
    match node {
        ReNode::Literal(c) if c.is_ascii_alphabetic() => {
            *node = ReNode::Class {
                members: vec![
                    ClassMember::Single(c.to_ascii_lowercase()),
                    ClassMember::Single(c.to_ascii_uppercase()),
                ],
                negated: false,
            };
        }
        ReNode::Class { members, .. } => {
            let mut extra: Vec<ClassMember> = Vec::new();
            for m in members.iter() {
                match m {
                    ClassMember::Single(c) if c.is_ascii_alphabetic() => {
                        extra.push(ClassMember::Single(c.to_ascii_lowercase()));
                        extra.push(ClassMember::Single(c.to_ascii_uppercase()));
                    }
                    ClassMember::Range(a, b)
                        if a.is_ascii_alphabetic() && b.is_ascii_alphabetic() =>
                    {
                        extra.push(ClassMember::Range(
                            a.to_ascii_lowercase(),
                            b.to_ascii_lowercase(),
                        ));
                        extra.push(ClassMember::Range(
                            a.to_ascii_uppercase(),
                            b.to_ascii_uppercase(),
                        ));
                    }
                    _ => {}
                }
            }
            members.extend(extra);
        }
        ReNode::Quant { inner, .. }
        | ReNode::Lookahead { inner, .. }
        | ReNode::Group { inner, .. } => fold_case(inner),
        ReNode::Concat(items) | ReNode::Alt(items) => {
            for it in items.iter_mut() {
                fold_case(it);
            }
        }
        // The `~*` path folds both sides of the backref comparison at match time.
        ReNode::Backref { ci, .. } => *ci = true,
        _ => {}
    }
}

pub(crate) fn re_compile(pat: &str) -> Result<ReNode, ReErr> {
    let all: Vec<char> = pat.chars().collect();
    // Leading inline option group `(?flags)` applies to the whole pattern. `i`
    // (case-insensitive) and `x` (extended / whitespace-ignoring) change
    // matching; the rest (m/s/n/…) are accepted and ignored. A `(?:…)`
    // non-capturing group is NOT an option group (its `:` isn't a flag letter)
    // — it is handled in re_parse_atom, so this leading-flag scan skips it.
    let mut fold = false;
    let mut extended = false;
    let mut start = 0;
    if all.len() >= 3 && all[0] == '(' && all[1] == '?' {
        if let Some(close) = all[2..].iter().position(|&c| c == ')') {
            let flags = &all[2..2 + close];
            if !flags.is_empty() && flags.iter().all(|c| "bceimnpqstwx".contains(*c)) {
                fold = flags.contains(&'i');
                extended = flags.contains(&'x');
                start = 2 + close + 1;
            }
        }
    }
    // `x` extended mode: unescaped whitespace outside a
    // character class is ignored and `#` starts a comment to end-of-line, so a
    // pattern can be laid out readably. Previously `x` was silently dropped,
    // which made a spaced-out pattern fail to match instead of matching.
    let body: Vec<char> = if extended {
        strip_regex_extended_whitespace(&all[start..])
    } else {
        all[start..].to_vec()
    };
    let mut p = 0;
    // 1-based capturing-group counter, assigned left-to-right
    // as `(` groups are parsed. Group 0 is the whole match (handled by re_find).
    let mut ng = 1usize;
    let mut n = re_parse_alt(&body, &mut p, 0, &mut ng)?;
    if p != body.len() {
        return Err(ReErr::TypeMismatch {
            detail: format!("regex compile: trailing chars at pos {p} in {pat:?}"),
        });
    }
    if fold {
        fold_case(&mut n);
    }
    Ok(n)
}

/// implement the regex `x` (extended) flag: drop
/// unescaped whitespace and `#`-to-EOL comments, but keep whitespace that is
/// escaped (`\ `) or inside a `[...]` character class, matching PG / POSIX ARE.
pub(crate) fn strip_regex_extended_whitespace(chars: &[char]) -> Vec<char> {
    let mut out = Vec::with_capacity(chars.len());
    let mut in_class = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && i + 1 < chars.len() {
            // Escaped pair is literal — keep both characters verbatim.
            out.push(c);
            out.push(chars[i + 1]);
            i += 2;
            continue;
        }
        if in_class {
            out.push(c);
            if c == ']' {
                in_class = false;
            }
            i += 1;
            continue;
        }
        match c {
            '[' => {
                in_class = true;
                out.push(c);
            }
            ' ' | '\t' | '\n' | '\r' | '\x0c' => {} // ignore unescaped whitespace
            '#' => {
                // Comment to end-of-line.
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            _ => out.push(c),
        }
        i += 1;
    }
    out
}

pub(crate) fn re_parse_alt(
    chars: &[char],
    p: &mut usize,
    depth: u32,
    ng: &mut usize,
) -> Result<ReNode, ReErr> {
    // bound group nesting so `"((((…"` can't
    // blow the parser's own recursion stack.
    if depth > PARSE_DEPTH_LIMIT {
        return Err(ReErr::TypeMismatch {
            detail: "invalid regular expression: regular expression is too complex".into(),
        });
    }
    let mut branches = vec![re_parse_concat(chars, p, depth, ng)?];
    while *p < chars.len() && chars[*p] == '|' {
        *p += 1;
        branches.push(re_parse_concat(chars, p, depth, ng)?);
    }
    if branches.len() == 1 { Ok(branches.pop().unwrap()) } else { Ok(ReNode::Alt(branches)) }
}

// LOC-WAIVER: vendored spg ERE engine core (byte-identical fork); splitting upstream's tested matcher/parser injects bugs without readability gain.
pub(crate) fn re_parse_concat(
    chars: &[char],
    p: &mut usize,
    depth: u32,
    ng: &mut usize,
) -> Result<ReNode, ReErr> {
    let mut items: Vec<ReNode> = Vec::new();
    while *p < chars.len() {
        let c = chars[*p];
        if c == '|' || c == ')' {
            break;
        }
        let atom = re_parse_atom(chars, p, depth, ng)?;
        // Optional quantifier suffix.
        let quantified = if *p < chars.len() {
            match chars[*p] {
                '*' => {
                    *p += 1;
                    // A trailing `?` makes the quantifier lazy
                    // (non-greedy): `X*?`. Consume it and record the
                    // laziness on the node.
                    let greedy = !consume_lazy_suffix(chars, p);
                    ReNode::Quant { inner: Box::new(atom), min: 0, max: None, greedy }
                }
                '+' => {
                    *p += 1;
                    let greedy = !consume_lazy_suffix(chars, p);
                    ReNode::Quant { inner: Box::new(atom), min: 1, max: None, greedy }
                }
                '?' => {
                    *p += 1;
                    // `X??` — lazy optional. The second `?` is the
                    // laziness marker, not a literal.
                    let greedy = !consume_lazy_suffix(chars, p);
                    ReNode::Quant { inner: Box::new(atom), min: 0, max: Some(1), greedy }
                }
                '{' => {
                    // counted repetition. Only a
                    // well-formed `{m}` / `{m,}` / `{m,n}` becomes a
                    // quantifier; a stray `{` (e.g. `foo{bar`) is left
                    // as an ordinary literal (PG/ERE semantics), so
                    // existing patterns that used `{` literally are
                    // unaffected.
                    match re_parse_bound(chars, p)? {
                        Some((min, max)) => {
                            // A trailing `?` makes the counted
                            // repetition lazy: `X{m,n}?`.
                            let greedy = !consume_lazy_suffix(chars, p);
                            ReNode::Quant { inner: Box::new(atom), min, max, greedy }
                        }
                        None => atom,
                    }
                }
                _ => atom,
            }
        } else {
            atom
        };
        items.push(quantified);
    }
    if items.len() == 1 { Ok(items.pop().unwrap()) } else { Ok(ReNode::Concat(items)) }
}

// LOC-WAIVER: vendored spg ERE engine core (byte-identical fork); splitting upstream's tested matcher/parser injects bugs without readability gain.
pub(crate) fn re_parse_atom(
    chars: &[char],
    p: &mut usize,
    depth: u32,
    ng: &mut usize,
) -> Result<ReNode, ReErr> {
    let c = chars[*p];
    match c {
        '(' => {
            *p += 1;
            // `(?...)` prefixes: `(?:` non-capturing group, `(?=`/`(?!` lookahead
            // assertions. (Capturing `(...)` groups are matched transparently
            // today; per-group capture extraction — needed for regexp_match
            // arrays / substring(from pattern) / `\N` in regexp_replace — is a
            // separate D.9 slice.)
            let mut lookahead: Option<bool> = None;
            // A capturing group unless it's `(?:…)` or a lookaround `(?=`/`(?!`.
            let mut capturing = true;
            if *p + 1 < chars.len() && chars[*p] == '?' {
                match chars[*p + 1] {
                    ':' => {
                        capturing = false;
                        *p += 2;
                    }
                    '=' => {
                        lookahead = Some(false);
                        capturing = false;
                        *p += 2;
                    }
                    '!' => {
                        lookahead = Some(true);
                        capturing = false;
                        *p += 2;
                    }
                    // any other `(?x` form here is NOT
                    // part of PG's ARE syntax (leading `(?flags)` options are
                    // consumed before parsing; PCRE named groups `(?P<n>` /
                    // `(?<n>` and atomic `(?>` don't exist in ARE).
                    // Previously the `?` fell through as a LITERAL inside a
                    // plain capturing group, so `(?<first>h)` silently
                    // matched nothing — a silent-wrong for callers expecting
                    // either PCRE behaviour or PG's error. Match PG's two
                    // messages: a letter reads as a (bad) embedded option;
                    // anything else is a `?` with no quantifier operand.
                    c => {
                        let msg = if c.is_ascii_alphabetic() {
                            "invalid regular expression: invalid embedded option"
                        } else {
                            "invalid regular expression: quantifier operand invalid"
                        };
                        return Err(ReErr::TypeMismatch { detail: msg.into() });
                    }
                }
            }
            // reserve this group's number BEFORE parsing the
            // inner so nested groups number in source order (`(a(b))` → 1, 2).
            let group_idx = if capturing {
                let idx = *ng;
                *ng += 1;
                Some(idx)
            } else {
                None
            };
            let inner = re_parse_alt(chars, p, depth + 1, ng)?;
            if *p >= chars.len() || chars[*p] != ')' {
                return Err(ReErr::TypeMismatch {
                    detail: "invalid regular expression: parentheses () not balanced".into(),
                });
            }
            *p += 1;
            match lookahead {
                Some(negative) => Ok(ReNode::Lookahead { negative, inner: Box::new(inner) }),
                None => match group_idx {
                    Some(idx) => Ok(ReNode::Group { idx, inner: Box::new(inner) }),
                    None => Ok(inner),
                },
            }
        }
        '[' => re_parse_class(chars, p),
        '.' => {
            *p += 1;
            Ok(ReNode::AnyChar)
        }
        '^' => {
            *p += 1;
            Ok(ReNode::Start)
        }
        '$' => {
            *p += 1;
            Ok(ReNode::End)
        }
        '\\' => {
            *p += 1;
            if *p >= chars.len() {
                return Err(ReErr::TypeMismatch {
                    detail: "regex compile: dangling backslash".into(),
                });
            }
            let esc = chars[*p];
            *p += 1;
            match esc {
                'd' => Ok(ReNode::Class {
                    members: vec![ClassMember::Range('0', '9')],
                    negated: false,
                }),
                'D' => {
                    Ok(ReNode::Class { members: vec![ClassMember::Range('0', '9')], negated: true })
                }
                'w' => Ok(ReNode::Class {
                    members: vec![
                        ClassMember::Range('a', 'z'),
                        ClassMember::Range('A', 'Z'),
                        ClassMember::Range('0', '9'),
                        ClassMember::Single('_'),
                    ],
                    negated: false,
                }),
                'W' => Ok(ReNode::Class {
                    members: vec![
                        ClassMember::Range('a', 'z'),
                        ClassMember::Range('A', 'Z'),
                        ClassMember::Range('0', '9'),
                        ClassMember::Single('_'),
                    ],
                    negated: true,
                }),
                's' => Ok(ReNode::Class { members: shortcut_members('s'), negated: false }),
                'S' => Ok(ReNode::Class { members: shortcut_members('s'), negated: true }),
                // PG ARE word-boundary assertions (constraint escapes,
                // regc_lex.c). Only `\m \M \y \Y` are word boundaries.
                // Verified against live PG18: `\b`/`\B` are NOT boundaries
                // in ARE — `\b` is the backspace char and `\B` is a literal
                // backslash (character-entry escapes). See below.
                'y' => Ok(ReNode::WordBoundary(WordBoundaryKind::Boundary)),
                'Y' => Ok(ReNode::WordBoundary(WordBoundaryKind::NonBoundary)),
                'm' => Ok(ReNode::WordBoundary(WordBoundaryKind::BegWord)),
                'M' => Ok(ReNode::WordBoundary(WordBoundaryKind::EndWord)),
                // PG ARE string anchors (constraint escapes, regc_lex.c):
                // `\A` matches only at the start of the string, `\Z` only
                // at the end. This engine has no newline-sensitive mode,
                // so `\A` ≡ `^` (Start) and `\Z` ≡ `$` (End) exactly.
                // Verified against live PG18: `'foobar' ~ '\Afoo'` = t,
                // `'xfoo' ~ '\Afoo'` = f, `'foobar' ~ 'bar\Z'` = t.
                'A' => Ok(ReNode::Start),
                'Z' => Ok(ReNode::End),
                // Character-entry escapes matching PG ARE semantics (regc_lex.c).
                'a' => Ok(ReNode::Literal('\u{07}')), // alert (BEL)
                'e' => Ok(ReNode::Literal('\u{1b}')), // escape (ESC)
                'f' => Ok(ReNode::Literal('\u{0c}')), // form feed
                'n' => Ok(ReNode::Literal('\n')),
                'r' => Ok(ReNode::Literal('\r')),
                't' => Ok(ReNode::Literal('\t')),
                'v' => Ok(ReNode::Literal('\u{0b}')), // vertical tab
                'b' => Ok(ReNode::Literal('\u{08}')), // backspace
                'B' => Ok(ReNode::Literal('\\')),     // literal backslash
                // `\xHH` (1–2 hex digits) and `\uHHHH` (4 hex digits) numeric
                // character escapes.
                'x' | 'u' => {
                    let want = if esc == 'x' { 2 } else { 4 };
                    let mut hex = String::new();
                    while hex.len() < want && *p < chars.len() && chars[*p].is_ascii_hexdigit() {
                        hex.push(chars[*p]);
                        *p += 1;
                    }
                    if hex.is_empty() {
                        return Err(ReErr::TypeMismatch {
                            detail: format!("regex compile: `\\{esc}` needs hex digits"),
                        });
                    }
                    let code = u32::from_str_radix(&hex, 16).map_err(|_| ReErr::TypeMismatch {
                        detail: "regex compile: bad numeric escape".into(),
                    })?;
                    Ok(ReNode::Literal(char::from_u32(code).unwrap_or('\u{fffd}')))
                }
                // `\1`..`\9` backreference. A
                // forward/unopened reference (`n >= *ng`, since `*ng` is the
                // next group number to assign) errors like PG.
                d @ '1'..='9' => {
                    let mut n = (d as usize) - ('0' as usize);
                    while *p < chars.len() && chars[*p].is_ascii_digit() {
                        n = n * 10 + ((chars[*p] as usize) - ('0' as usize));
                        *p += 1;
                    }
                    if n == 0 || n >= *ng {
                        return Err(ReErr::TypeMismatch {
                            detail: "invalid regular expression: invalid backreference number"
                                .into(),
                        });
                    }
                    Ok(ReNode::Backref { idx: n, ci: false })
                }
                other => Ok(ReNode::Literal(other)),
            }
        }
        other => {
            *p += 1;
            Ok(ReNode::Literal(other))
        }
    }
}
