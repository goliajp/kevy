//! Small pure helpers shared across the store modules.

pub(crate) fn norm_index(idx: i64, len: usize) -> Option<usize> {
    let len = len as i64;
    let i = if idx < 0 { idx + len } else { idx };
    if i < 0 || i >= len {
        None
    } else {
        Some(i as usize)
    }
}

/// Clamp a possibly-negative `[start, stop]` range to valid bounds (inclusive),
/// or `None` if the range is empty.
pub(crate) fn range_bounds(start: i64, stop: i64, len: usize) -> Option<(usize, usize)> {
    if len == 0 {
        return None;
    }
    let len = len as i64;
    let s = (if start < 0 { start + len } else { start }).max(0);
    let e = (if stop < 0 { stop + len } else { stop }).min(len - 1);
    if s > e || s >= len {
        None
    } else {
        Some((s as usize, e as usize))
    }
}

/// Strict base-10 `i64` parse over raw bytes (allows a leading `+`/`-`).
pub(crate) fn parse_i64(b: &[u8]) -> Option<i64> {
    std::str::from_utf8(b).ok()?.parse::<i64>().ok()
}

/// Parse a finite f64 from raw bytes (rejects NaN/inf for value storage).
pub(crate) fn parse_f64(b: &[u8]) -> Option<f64> {
    let f: f64 = std::str::from_utf8(b).ok()?.trim().parse().ok()?;
    f.is_finite().then_some(f)
}

/// Redis-style glob match (`*`, `?`, `[...]` classes with ranges/`^`, `\` escape).
pub fn glob_match(pat: &[u8], s: &[u8]) -> bool {
    glob(pat, s)
}

fn glob(mut p: &[u8], mut s: &[u8]) -> bool {
    while let Some(&c) = p.first() {
        match c {
            b'*' => return glob_star(p, s),
            b'?' => {
                if s.is_empty() {
                    return false;
                }
                s = &s[1..];
                p = &p[1..];
            }
            b'[' => {
                let Some(ch) = s.first().copied() else {
                    return false;
                };
                let (matched, rest) = match_class(&p[1..], ch);
                if !matched {
                    return false;
                }
                p = rest;
                s = &s[1..];
            }
            b'\\' if p.len() >= 2 => {
                if s.first() != Some(&p[1]) {
                    return false;
                }
                s = &s[1..];
                p = &p[2..];
            }
            _ => {
                if s.first() != Some(&c) {
                    return false;
                }
                s = &s[1..];
                p = &p[1..];
            }
        }
    }
    s.is_empty()
}

/// Handles the `*` arm of `glob`: collapse a run of `*`s, then try every tail.
fn glob_star(mut p: &[u8], s: &[u8]) -> bool {
    while p.get(1) == Some(&b'*') {
        p = &p[1..];
    }
    if p.len() == 1 {
        return true; // trailing '*' matches the rest
    }
    let tail = &p[1..];
    (0..=s.len()).any(|i| glob(tail, &s[i..]))
}

/// Match one char against a `[...]` class; return `(matched, pattern_after_class)`.
fn match_class(p: &[u8], ch: u8) -> (bool, &[u8]) {
    let mut i = 0;
    let negate = p.first() == Some(&b'^');
    if negate {
        i += 1;
    }
    let mut matched = false;
    while i < p.len() && p[i] != b']' {
        if p[i] == b'\\' && i + 1 < p.len() {
            matched |= p[i + 1] == ch;
            i += 2;
        } else if i + 2 < p.len() && p[i + 1] == b'-' && p[i + 2] != b']' {
            let (lo, hi) = if p[i] <= p[i + 2] {
                (p[i], p[i + 2])
            } else {
                (p[i + 2], p[i])
            };
            matched |= (lo..=hi).contains(&ch);
            i += 3;
        } else {
            matched |= p[i] == ch;
            i += 1;
        }
    }
    if i < p.len() {
        i += 1; // skip ']'
    }
    (matched ^ negate, &p[i..])
}

/// Format a number the way Redis does: integral values without a decimal point.
pub(crate) fn fmt_num(v: f64) -> Vec<u8> {
    if v == v.trunc() && v.abs() < 1e17 {
        (v as i64).to_string().into_bytes()
    } else {
        format!("{v}").into_bytes()
    }
}
