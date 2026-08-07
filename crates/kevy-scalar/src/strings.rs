//! String functions: case, measure, concatenation, the trim family,
//! and character substitution. The slice/pad/position family lives in
//! `strings_slice` — same probe corpus, split at the 500-LOC line.
//!
//! Everything is codepoint-counted (PG counts characters, not bytes),
//! and strict NULL propagation is enforced here for every function
//! except `concat`/`concat_ws`, which skip NULL data arguments by
//! design (probe 36/37).

use crate::{Scalar, ScalarError, strings_slice};

pub(crate) fn eval(name: &str, args: &[Scalar]) -> Result<Scalar, ScalarError> {
    match name {
        "lower" => map_text("lower", args, |s| s.to_lowercase()),
        "upper" => map_text("upper", args, |s| s.to_uppercase()),
        "initcap" => map_text("initcap", args, initcap),
        "length" | "char_length" | "character_length" => {
            let Some([s]) = strict1("length", args)? else {
                return Ok(Scalar::Null);
            };
            Ok(Scalar::Int(s.chars().count() as i64))
        }
        "reverse" => map_text("reverse", args, |s| s.chars().rev().collect()),
        "concat" => Ok(Scalar::Text(concat_parts(args))),
        "concat_ws" => concat_ws(args),
        "trim" | "btrim" => trim_family("btrim", args, true, true),
        "ltrim" => trim_family("ltrim", args, true, false),
        "rtrim" => trim_family("rtrim", args, false, true),
        "replace" => replace(args),
        "translate" => translate(args),
        "repeat" => repeat(args),
        "format" => format_fn(args),
        _ => strings_slice::eval(name, args),
    }
}

/// One text argument, strict NULL: `Ok(None…)` is expressed by the
/// caller matching — here we return the borrowed text or signal NULL
/// via the sentinel `Err`-free path. Shaped as `Option` so callers
/// write `let [s] = strict1(…)? else { return Null }`.
fn strict1<'a>(func: &'static str, args: &'a [Scalar]) -> Result<Option<[&'a str; 1]>, ScalarError> {
    match args {
        [Scalar::Null] => Ok(None),
        [Scalar::Text(s)] => Ok(Some([s.as_str()])),
        [_] => Err(ScalarError::Type { func, arg: 0 }),
        _ => Err(ScalarError::Arity { func, got: args.len() }),
    }
}

fn map_text(
    func: &'static str,
    args: &[Scalar],
    f: impl Fn(&str) -> String,
) -> Result<Scalar, ScalarError> {
    let Some([s]) = strict1(func, args)? else {
        return Ok(Scalar::Null);
    };
    Ok(Scalar::Text(f(s)))
}

/// Uppercase the first letter of each word, lowercase the rest; a word
/// starts after any non-alphanumeric character (PG's rule).
fn initcap(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut word_start = true;
    for c in s.chars() {
        if c.is_alphanumeric() {
            if word_start {
                out.extend(c.to_uppercase());
            } else {
                out.extend(c.to_lowercase());
            }
            word_start = false;
        } else {
            out.push(c);
            word_start = true;
        }
    }
    out
}

/// PG renders non-text arguments with their text output form; the
/// subset this crate speaks: ints, floats (minimal form), booleans as
/// `t`/`f`. NULLs are skipped by `concat`, poisonous nowhere here.
pub(crate) fn to_text(v: &Scalar) -> String {
    match v {
        Scalar::Null => String::new(),
        Scalar::Int(i) => i.to_string(),
        Scalar::Float(f) => crate::math::render_float(*f),
        Scalar::Text(s) => s.clone(),
        Scalar::Bool(b) => (if *b { "t" } else { "f" }).to_string(),
        Scalar::Timestamp(us) => crate::datetime_fmt::render_timestamp(*us),
        Scalar::Date(d) => crate::datetime_fmt::render_date(*d),
        Scalar::Interval { months, days, micros } => {
            crate::datetime_fmt::render_interval(*months, *days, *micros)
        }
    }
}

fn concat_parts(args: &[Scalar]) -> String {
    let mut out = String::new();
    for a in args {
        if !a.is_null() {
            out.push_str(&to_text(a));
        }
    }
    out
}

/// `concat_ws(sep, …)`: NULL separator poisons the whole result; NULL
/// data args are skipped WITHOUT inserting their separator (probe 37).
fn concat_ws(args: &[Scalar]) -> Result<Scalar, ScalarError> {
    let Some((sep, rest)) = args.split_first() else {
        return Err(ScalarError::Arity { func: "concat_ws", got: 0 });
    };
    let sep = match sep {
        Scalar::Null => return Ok(Scalar::Null),
        Scalar::Text(s) => s.as_str(),
        _ => return Err(ScalarError::Type { func: "concat_ws", arg: 0 }),
    };
    let parts: Vec<String> = rest.iter().filter(|a| !a.is_null()).map(to_text).collect();
    Ok(Scalar::Text(parts.join(sep)))
}

/// The trim family strips a character SET (any codepoint in `chars`),
/// not a substring; the default set is ASCII space ONLY — tab/newline
/// survive a bare `trim` (probe 39).
fn trim_family(
    func: &'static str,
    args: &[Scalar],
    left: bool,
    right: bool,
) -> Result<Scalar, ScalarError> {
    let (s, set) = match args {
        [Scalar::Null] | [Scalar::Null, _] | [_, Scalar::Null] => return Ok(Scalar::Null),
        [Scalar::Text(s)] => (s.as_str(), " ".to_string()),
        [Scalar::Text(s), Scalar::Text(c)] => (s.as_str(), c.clone()),
        [_] | [_, _] => return Err(ScalarError::Type { func, arg: 0 }),
        _ => return Err(ScalarError::Arity { func, got: args.len() }),
    };
    let in_set = |c: char| set.contains(c);
    let out = match (left, right) {
        (true, true) => s.trim_matches(in_set),
        (true, false) => s.trim_start_matches(in_set),
        (false, true) => s.trim_end_matches(in_set),
        (false, false) => s,
    };
    Ok(Scalar::Text(out.to_string()))
}

/// Every non-overlapping occurrence, left to right; the replacement is
/// not re-scanned; an empty `from` answers the input unchanged.
fn replace(args: &[Scalar]) -> Result<Scalar, ScalarError> {
    let Some([s, from, to]) = strict_text::<3>("replace", args)? else {
        return Ok(Scalar::Null);
    };
    if from.is_empty() {
        return Ok(Scalar::Text(s.to_string()));
    }
    Ok(Scalar::Text(s.replace(from, to)))
}

/// Positional codepoint mapping: the Nth char of `from` maps to the
/// Nth of `to`; chars of `from` beyond `to`'s length are deleted.
fn translate(args: &[Scalar]) -> Result<Scalar, ScalarError> {
    let Some([s, from, to]) = strict_text::<3>("translate", args)? else {
        return Ok(Scalar::Null);
    };
    let to: Vec<char> = to.chars().collect();
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match from.chars().position(|f| f == c) {
            None => out.push(c),
            Some(i) => {
                if let Some(&r) = to.get(i) {
                    out.push(r);
                }
            }
        }
    }
    Ok(Scalar::Text(out))
}

fn repeat(args: &[Scalar]) -> Result<Scalar, ScalarError> {
    let (s, n) = match args {
        [Scalar::Null, _] | [_, Scalar::Null] => return Ok(Scalar::Null),
        [Scalar::Text(s), Scalar::Int(n)] => (s.as_str(), *n),
        [_, _] => return Err(ScalarError::Type { func: "repeat", arg: 0 }),
        _ => return Err(ScalarError::Arity { func: "repeat", got: args.len() }),
    };
    if n <= 0 {
        return Ok(Scalar::Text(String::new()));
    }
    Ok(Scalar::Text(s.repeat(n as usize)))
}

/// PG `format(fmt, args…)` — the sprintf-shaped subset probe 32 pins:
/// `%s` (text form, NULL prints nothing), `%L` (quoted literal with
/// `''` doubling, NULL prints bare `NULL`), `%I` (identifier, quoted
/// only when needed, NULL is an error), `%%`, and positional `%n$X`.
/// Unknown specifiers error — PG refuses them too.
fn format_fn(args: &[Scalar]) -> Result<Scalar, ScalarError> {
    const FUNC: &str = "format";
    let Some((fmt, data)) = args.split_first() else {
        return Err(ScalarError::Arity { func: FUNC, got: 0 });
    };
    let fmt = match fmt {
        Scalar::Null => return Ok(Scalar::Null),
        Scalar::Text(s) => s.as_str(),
        _ => return Err(ScalarError::Type { func: FUNC, arg: 0 }),
    };
    let mut out = String::with_capacity(fmt.len());
    let mut it = fmt.chars().peekable();
    let mut next = 0usize;
    while let Some(c) = it.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let (pos, spec) = parse_spec(&mut it)?;
        if spec == '%' {
            out.push('%');
            continue;
        }
        let idx = pos.unwrap_or_else(|| {
            let i = next;
            next += 1;
            i
        });
        let v = data.get(idx).ok_or(ScalarError::Domain {
            func: FUNC,
            what: "too few arguments for format()",
        })?;
        format_spec(spec, v, &mut out)?;
    }
    Ok(Scalar::Text(out))
}

/// After a `%`: the optional `n$` positional prefix (0-based on
/// return) and the specifier character itself.
fn parse_spec(
    it: &mut std::iter::Peekable<std::str::Chars<'_>>,
) -> Result<(Option<usize>, char), ScalarError> {
    const FUNC: &str = "format";
    let mut digits = String::new();
    while it.peek().is_some_and(char::is_ascii_digit) {
        digits.push(it.next().expect("peeked"));
    }
    let positional = !digits.is_empty() && it.peek() == Some(&'$');
    if positional {
        it.next();
    } else if !digits.is_empty() {
        return Err(ScalarError::Domain { func: FUNC, what: "unrecognized format() type specifier" });
    }
    let spec = it.next().ok_or(ScalarError::Domain {
        func: FUNC,
        what: "unterminated format() type specifier",
    })?;
    let pos = positional.then(|| digits.parse::<usize>().unwrap_or(0).wrapping_sub(1));
    Ok((pos, spec))
}

fn format_spec(spec: char, v: &Scalar, out: &mut String) -> Result<(), ScalarError> {
    const FUNC: &str = "format";
    match spec {
        's' => out.push_str(&to_text(v)), // NULL prints nothing
        'L' => {
            if v.is_null() {
                out.push_str("NULL");
            } else {
                out.push('\'');
                out.push_str(&to_text(v).replace('\'', "''"));
                out.push('\'');
            }
        }
        'I' => {
            if v.is_null() {
                return Err(ScalarError::Domain {
                    func: FUNC,
                    what: "null values cannot be formatted as an SQL identifier",
                });
            }
            let id = to_text(v);
            let bare = !id.is_empty()
                && id.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c == '_')
                && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
            if bare {
                out.push_str(&id);
            } else {
                out.push('"');
                out.push_str(&id.replace('"', "\"\""));
                out.push('"');
            }
        }
        _ => {
            return Err(ScalarError::Domain {
                func: FUNC,
                what: "unrecognized format() type specifier",
            });
        }
    }
    Ok(())
}

/// N text arguments, strict NULL (any NULL → the caller answers NULL).
pub(crate) fn strict_text<'a, const N: usize>(
    func: &'static str,
    args: &'a [Scalar],
) -> Result<Option<[&'a str; N]>, ScalarError> {
    if args.len() != N {
        return Err(ScalarError::Arity { func, got: args.len() });
    }
    if args.iter().any(Scalar::is_null) {
        return Ok(None);
    }
    let mut out = [""; N];
    for (i, a) in args.iter().enumerate() {
        match a {
            Scalar::Text(s) => out[i] = s.as_str(),
            _ => return Err(ScalarError::Type { func, arg: i }),
        }
    }
    Ok(Some(out))
}
