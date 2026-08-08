//! (vendored regex engine — see the parent module.)
#![allow(clippy::all, clippy::pedantic)]
use super::*;

pub(crate) fn re_parse_class(chars: &[char], p: &mut usize) -> Result<ReNode, ReErr> {
    debug_assert_eq!(chars.get(*p), Some(&'['));
    *p += 1;
    let mut negated = false;
    if *p < chars.len() && chars[*p] == '^' {
        negated = true;
        *p += 1;
    }
    let mut members: Vec<ClassMember> = Vec::new();
    let mut first = true;
    while *p < chars.len() {
        let c = chars[*p];
        // `]` closes the class — except in the first member position,
        // where POSIX/PG treat it as a literal `]`.
        if c == ']' && !first {
            *p += 1; // consume closing ]
            return Ok(ReNode::Class { members, negated });
        }
        first = false;

        // POSIX class `[:name:]` — requires a literal `[` (inside the
        // outer bracket) immediately followed by `:`.
        if c == '[' && chars.get(*p + 1) == Some(&':') {
            let mut q = *p + 2;
            let name_start = q;
            while q < chars.len() && chars[q] != ':' {
                q += 1;
            }
            // Must close with `:]`. A missing `:]`, or a `[:^name:]`
            // (which scans a name of "^name" → unknown), is rejected as
            // an invalid character class — the same error PG raises.
            if q + 1 >= chars.len() || chars[q + 1] != ']' {
                return Err(ReErr::TypeMismatch {
                    detail: "invalid regular expression: invalid character class".into(),
                });
            }
            let name: String = chars[name_start..q].iter().collect();
            members.extend(posix_class_members(&name)?);
            *p = q + 2; // consume through `:]`
            continue;
        }

        // Escape inside the class: positive shortcuts expand inline, the
        // complements become a NotInSet member, char escapes fold to a
        // literal.
        if c == '\\' && *p + 1 < chars.len() {
            let esc = chars[*p + 1];
            *p += 2;
            match esc {
                'd' | 'w' | 's' => members.extend(shortcut_members(esc)),
                'D' => members.push(ClassMember::NotInSet(shortcut_members('d'))),
                'W' => members.push(ClassMember::NotInSet(shortcut_members('w'))),
                'S' => members.push(ClassMember::NotInSet(shortcut_members('s'))),
                't' => members.push(ClassMember::Single('\t')),
                'n' => members.push(ClassMember::Single('\n')),
                'r' => members.push(ClassMember::Single('\r')),
                'f' => members.push(ClassMember::Single('\u{0c}')),
                'v' => members.push(ClassMember::Single('\u{0b}')),
                'b' => members.push(ClassMember::Single('\u{08}')), // backspace
                other => members.push(ClassMember::Single(other)),
            }
            continue;
        }

        // Ordinary char, possibly the start of a range `a-z`. A trailing
        // `-` (next char is the closing `]`) is a literal `-`.
        let start = c;
        *p += 1;
        if *p + 1 < chars.len() && chars[*p] == '-' && chars[*p + 1] != ']' {
            let end = chars[*p + 1];
            *p += 2;
            // v7.39 (round 772, F31 J2) — a REVERSED range (`[z-a]`)
            // is PG's "invalid regular expression: invalid character
            // range" (measured); the old parser recorded it and
            // matched nothing, silently.
            if end < start {
                return Err(ReErr::TypeMismatch {
                    detail: "invalid regular expression: invalid character range".into(),
                });
            }
            members.push(ClassMember::Range(start, end));
        } else {
            members.push(ClassMember::Single(start));
        }
    }
    // Fell off the end of the pattern without a closing `]`.
    Err(ReErr::TypeMismatch {
        detail: "invalid regular expression: brackets [] not balanced".into(),
    })
}

/// v7.37.16 Epic Rx P0 — parse a `{m}` / `{m,}` / `{m,n}` counted
/// repetition beginning at `chars[*p] == '{'`.
///
/// * `Ok(Some((min, max)))` — a well-formed bound; `*p` is advanced
///   past the closing `}`. `max == None` means `{m,}` (unbounded).
/// * `Ok(None)` — the text at `*p` is *not* a valid bound; `*p` is
///   left unchanged so the caller keeps `{` as an ordinary literal
///   (PG/ERE semantics).
/// * `Err(..)` — the bound is well-formed but exceeds `REPEAT_MAX`
///   (PG REG_ETOOBIG) or is inverted (`n < m`).
pub(crate) fn re_parse_bound(
    chars: &[char],
    p: &mut usize,
) -> Result<Option<(usize, Option<usize>)>, ReErr> {
    debug_assert_eq!(chars.get(*p), Some(&'{'));
    let mut q = *p + 1;
    // Minimum count — at least one digit is required for `{` to open
    // a bound; otherwise it is a literal brace.
    let (min, min_digits) = re_scan_count(chars, &mut q);
    if min_digits == 0 {
        return Ok(None);
    }
    // Optional `,max`.
    let mut max = Some(min);
    if q < chars.len() && chars[q] == ',' {
        q += 1;
        let (m, m_digits) = re_scan_count(chars, &mut q);
        max = if m_digits == 0 { None } else { Some(m) };
    }
    // A bound must close with `}`; anything else falls back to literal.
    if q >= chars.len() || chars[q] != '}' {
        return Ok(None);
    }
    // Well-formed bound — enforce PG's repetition ceiling before we
    // hand a huge count to the matcher.
    let repeat_max = REPEAT_MAX as usize;
    if min > repeat_max || matches!(max, Some(mx) if mx > repeat_max) {
        return Err(ReErr::TypeMismatch {
            detail: format!(
                "invalid regular expression: regular expression is too complex \
                 (repetition count exceeds {REPEAT_MAX})"
            ),
        });
    }
    if let Some(mx) = max {
        if mx < min {
            return Err(ReErr::TypeMismatch {
                detail: "invalid regular expression: {m,n} quantifier with n < m".into(),
            });
        }
    }
    *p = q + 1; // consume through the closing `}`
    Ok(Some((min, max)))
}

/// Scan a run of ASCII digits starting at `*p`, returning the value
/// and the number of digits consumed. The value saturates at
/// `REPEAT_MAX + 1` so an absurdly long digit string (e.g.
/// `{99999999999999999999}`) cannot overflow `usize` — it stays just
/// above the ceiling so the caller rejects it.
pub(crate) fn re_scan_count(chars: &[char], p: &mut usize) -> (usize, usize) {
    let ceiling = REPEAT_MAX as usize + 1;
    let mut val: usize = 0;
    let mut digits = 0usize;
    while *p < chars.len() && chars[*p].is_ascii_digit() {
        let d = (chars[*p] as u8 - b'0') as usize;
        val = val.saturating_mul(10).saturating_add(d).min(ceiling);
        digits += 1;
        *p += 1;
    }
    (val, digits)
}

/// If the character at `*p` is a `?` (a lazy / non-greedy marker
/// immediately after a quantifier), consume it and return `true`;
/// otherwise leave `*p` unchanged and return `false`. The caller sets
/// `greedy = !consume_lazy_suffix(...)`.
pub(crate) fn consume_lazy_suffix(chars: &[char], p: &mut usize) -> bool {
    if *p < chars.len() && chars[*p] == '?' {
        *p += 1;
        true
    } else {
        false
    }
}
