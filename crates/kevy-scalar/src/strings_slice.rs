//! The slice/pad/position half of the string library: `split_part`,
//! `lpad`/`rpad`, `strpos`/`position`, `left`/`right`, `substr`.
//! Split from `strings.rs` at the 500-LOC line; same corpus, same
//! codepoint-counted rules.

use crate::strings::strict_text;
use crate::{Scalar, ScalarError};

pub(crate) fn eval(name: &str, args: &[Scalar]) -> Result<Scalar, ScalarError> {
    match name {
        "split_part" => split_part(args),
        "lpad" => pad("lpad", args, true),
        "rpad" => pad("rpad", args, false),
        "strpos" => strpos(args, false),
        // position(needle, haystack) — argument order is the REVERSE
        // of strpos (probe 44's header note); the sql face hands
        // `position(x IN y)` through in written order.
        "position" => strpos(args, true),
        "left" => left_right(args, true),
        "right" => left_right(args, false),
        "substr" | "substring" => substr(args),
        _ => Err(ScalarError::UnknownFunction(name.to_string())),
    }
}

/// 1-indexed; negative counts from the end (PG 14+); out of range →
/// empty; n=0 errors; an EMPTY delimiter answers the whole string as
/// field 1 (probe 41: the old error pin was wrong).
fn split_part(args: &[Scalar]) -> Result<Scalar, ScalarError> {
    let (s, delim, n) = match args {
        [Scalar::Null, ..] | [_, Scalar::Null, _] | [_, _, Scalar::Null] => {
            return Ok(Scalar::Null);
        }
        [Scalar::Text(s), Scalar::Text(d), Scalar::Int(n)] => (s.as_str(), d.as_str(), *n),
        [_, _, _] => return Err(ScalarError::Type { func: "split_part", arg: 0 }),
        _ => return Err(ScalarError::Arity { func: "split_part", got: args.len() }),
    };
    if n == 0 {
        return Err(ScalarError::Domain {
            func: "split_part",
            what: "field position must not be zero",
        });
    }
    let fields: Vec<&str> = if delim.is_empty() { vec![s] } else { s.split(delim).collect() };
    let idx = if n > 0 {
        (n - 1) as usize
    } else {
        let back = (-n) as usize;
        if back > fields.len() {
            return Ok(Scalar::Text(String::new()));
        }
        fields.len() - back
    };
    Ok(Scalar::Text(fields.get(idx).copied().unwrap_or("").to_string()))
}

/// Target is a codepoint count; a too-long input truncates keeping the
/// LEFT side (both pads); the fill cycles; empty fill that would need
/// padding answers the input verbatim (probe 43).
fn pad(func: &'static str, args: &[Scalar], left: bool) -> Result<Scalar, ScalarError> {
    let (s, len, fill) = match args {
        [Scalar::Null, ..] | [_, Scalar::Null] | [_, Scalar::Null, _] | [_, _, Scalar::Null] => {
            return Ok(Scalar::Null);
        }
        [Scalar::Text(s), Scalar::Int(n)] => (s.as_str(), *n, " "),
        [Scalar::Text(s), Scalar::Int(n), Scalar::Text(f)] => (s.as_str(), *n, f.as_str()),
        [_, _] | [_, _, _] => return Err(ScalarError::Type { func, arg: 0 }),
        _ => return Err(ScalarError::Arity { func, got: args.len() }),
    };
    if len <= 0 {
        return Ok(Scalar::Text(String::new()));
    }
    let len = len as usize;
    let have = s.chars().count();
    if have >= len {
        return Ok(Scalar::Text(s.chars().take(len).collect()));
    }
    if fill.is_empty() {
        return Ok(Scalar::Text(s.to_string()));
    }
    let padding: String = fill.chars().cycle().take(len - have).collect();
    Ok(Scalar::Text(if left { padding + s } else { s.to_string() + &padding }))
}

/// 1-indexed codepoint position; 0 when absent; empty needle → 1.
fn strpos(args: &[Scalar], reversed: bool) -> Result<Scalar, ScalarError> {
    let func: &'static str = if reversed { "position" } else { "strpos" };
    let Some([a, b]) = strict_text::<2>(func, args)? else {
        return Ok(Scalar::Null);
    };
    let (hay, needle) = if reversed { (b, a) } else { (a, b) };
    if needle.is_empty() {
        return Ok(Scalar::Int(1));
    }
    match hay.find(needle) {
        None => Ok(Scalar::Int(0)),
        // Byte offset → 1-based codepoint position.
        Some(byte) => Ok(Scalar::Int(hay[..byte].chars().count() as i64 + 1)),
    }
}

/// `left(s, n)` / `right(s, n)`: negative n means all-but-|n| chars
/// counted from the OPPOSITE side; n=0 → empty (probe 45).
fn left_right(args: &[Scalar], from_left: bool) -> Result<Scalar, ScalarError> {
    let func: &'static str = if from_left { "left" } else { "right" };
    let (s, n) = match args {
        [Scalar::Null, _] | [_, Scalar::Null] => return Ok(Scalar::Null),
        [Scalar::Text(s), Scalar::Int(n)] => (s.as_str(), *n),
        [_, _] => return Err(ScalarError::Type { func, arg: 0 }),
        _ => return Err(ScalarError::Arity { func, got: args.len() }),
    };
    let total = s.chars().count();
    let keep = if n >= 0 { (n as usize).min(total) } else { total.saturating_sub((-n) as usize) };
    let out: String = if from_left {
        s.chars().take(keep).collect()
    } else {
        s.chars().skip(total - keep).collect()
    };
    Ok(Scalar::Text(out))
}

/// `substr(s, start [, count])` — PG window arithmetic: the window is
/// `[start, start + count)` intersected with `[1, ∞)`; a start at or
/// below zero is legal and eats into the count; a negative count is
/// an error.
fn substr(args: &[Scalar]) -> Result<Scalar, ScalarError> {
    let (s, start, count) = match args {
        [Scalar::Null, ..] | [_, Scalar::Null] | [_, Scalar::Null, _] | [_, _, Scalar::Null] => {
            return Ok(Scalar::Null);
        }
        [Scalar::Text(s), Scalar::Int(p)] => (s.as_str(), *p, None),
        [Scalar::Text(s), Scalar::Int(p), Scalar::Int(c)] => (s.as_str(), *p, Some(*c)),
        [_, _] | [_, _, _] => return Err(ScalarError::Type { func: "substr", arg: 0 }),
        _ => return Err(ScalarError::Arity { func: "substr", got: args.len() }),
    };
    if let Some(c) = count
        && c < 0
    {
        return Err(ScalarError::Domain {
            func: "substr",
            what: "negative substring length not allowed",
        });
    }
    // Window in 1-based codepoint coordinates, then clamp to [1, ∞).
    let end = count.map(|c| start.saturating_add(c));
    let from = start.max(1);
    let out: String = match end {
        Some(e) if e <= from => String::new(),
        Some(e) => s.chars().skip((from - 1) as usize).take((e - from) as usize).collect(),
        None => s.chars().skip((from - 1) as usize).collect(),
    };
    Ok(Scalar::Text(out))
}
