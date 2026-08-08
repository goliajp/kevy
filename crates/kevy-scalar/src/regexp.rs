//! kevy's regexp scalar functions over the vendored engine
//! (`super::regex_engine`): the PG functions the pg_regress corpus
//! exercises — `regexp_replace`, `regexp_matches`,
//! `regexp_split_to_array` — each returning `Scalar::Text`. The array
//! returners render to PG's `{...}` text-array output, which is all
//! the probes ever observe (kevy's Scalar has no array variant, and
//! does not need one for this surface).
//!
//! The match/replace LOGIC mirrors spg's wrappers (arg parsing, the
//! `g`/`i` flags, capture-group replacement) but is rewritten over
//! `Scalar` rather than ported; the ENGINE underneath is the fork.

use crate::regex_engine as re;
use crate::{Scalar, ScalarError};

fn compile(func: &'static str, pat: &str, fold_i: bool) -> Result<re::ReNode, ScalarError> {
    let mut node = re::re_compile(pat).map_err(|re::ReErr::TypeMismatch { .. }| {
        ScalarError::Domain { func, what: "invalid regular expression" }
    })?;
    if fold_i {
        re::fold_case(&mut node);
    }
    Ok(node)
}

fn matcher_err(func: &'static str) -> impl Fn(re::ReErr) -> ScalarError {
    move |re::ReErr::TypeMismatch { .. }| ScalarError::Domain {
        func,
        what: "regular expression match failed",
    }
}

/// Dispatch entry — the names `lib::eval` routes here.
pub(crate) fn eval(name: &str, args: &[Scalar]) -> Result<Scalar, ScalarError> {
    match name {
        "regexp_replace" => regexp_replace(args),
        "regexp_matches" => regexp_matches(args),
        "regexp_split_to_array" => regexp_split_to_array(args),
        _ => Err(ScalarError::UnknownFunction(name.to_string())),
    }
}

fn text(v: &Scalar) -> Option<&str> {
    match v {
        Scalar::Text(s) => Some(s.as_str()),
        _ => None,
    }
}

/// PG's text-array output form: `{a,b,c}`, NULL elements as bare `NULL`,
/// and any element that is empty or carries `{ } , " \` or whitespace
/// double-quoted with `"`/`\` escaped (matches PG's `array_out`).
fn render_array(elems: &[Option<String>]) -> String {
    let mut out = String::from("{");
    for (i, e) in elems.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match e {
            None => out.push_str("NULL"),
            Some(s) => {
                let needs_quote = s.is_empty()
                    || s.eq_ignore_ascii_case("null")
                    || s.chars().any(|c| {
                        matches!(c, '{' | '}' | ',' | '"' | '\\') || c.is_whitespace()
                    });
                if needs_quote {
                    out.push('"');
                    for c in s.chars() {
                        if c == '"' || c == '\\' {
                            out.push('\\');
                        }
                        out.push(c);
                    }
                    out.push('"');
                } else {
                    out.push_str(s);
                }
            }
        }
    }
    out.push('}');
    out
}

/// `regexp_replace(text, pattern, replacement [, flags])` — the corpus
/// subset (kevy does not take the start/N positional args PG 15 added;
/// they refuse by arity rather than mis-answer).
fn regexp_replace(args: &[Scalar]) -> Result<Scalar, ScalarError> {
    const FUNC: &str = "regexp_replace";
    if args.len() < 3 || args.len() > 4 {
        return Err(ScalarError::Arity { func: FUNC, got: args.len() });
    }
    if args.iter().take(3).any(Scalar::is_null) || args.get(3).is_some_and(Scalar::is_null) {
        return Ok(Scalar::Null);
    }
    let flags = match args.get(3) {
        None => "",
        Some(Scalar::Text(f)) => f.as_str(),
        Some(_) => return Err(ScalarError::Type { func: FUNC, arg: 3 }),
    };
    let (Some(t), Some(pat), Some(repl)) = (text(&args[0]), text(&args[1]), text(&args[2])) else {
        return Err(ScalarError::Type { func: FUNC, arg: 0 });
    };
    let node = compile(FUNC, pat, flags.contains('i'))?;
    let global = flags.contains('g');
    let chars: Vec<char> = t.chars().collect();
    let ngroups = re::max_group(&node);
    let mut out = String::with_capacity(t.len());
    let (mut from, err) = (0usize, matcher_err(FUNC));
    loop {
        match re::re_find_caps(&node, &chars, from, ngroups).map_err(&err)? {
            Some(((s, e), caps)) => {
                out.extend(chars[from..s].iter());
                re::expand_replacement(repl, &chars, (s, e), &caps, &mut out);
                from = if e > from { e } else { e + 1 };
                if !global {
                    if from <= chars.len() {
                        out.extend(chars[from..].iter());
                    }
                    return Ok(Scalar::Text(out));
                }
                if from > chars.len() {
                    break;
                }
            }
            None => {
                out.extend(chars[from..].iter());
                break;
            }
        }
    }
    Ok(Scalar::Text(out))
}

/// One rendered `{...}` array per match (its capture groups, or the
/// whole match when the pattern has none). `regexp_matches` is a
/// set-returning function; this collects the rows and the caller maps
/// their cardinality onto the one-row fold face.
fn match_rows(
    func: &'static str,
    node: &re::ReNode,
    text: &str,
    global: bool,
) -> Result<Vec<String>, ScalarError> {
    let chars: Vec<char> = text.chars().collect();
    let ngroups = re::max_group(node);
    let mut rows: Vec<String> = Vec::new();
    let (mut from, err) = (0usize, matcher_err(func));
    while let Some(((s, e), caps)) = re::re_find_caps(node, &chars, from, ngroups).map_err(&err)? {
        let elems: Vec<Option<String>> = if ngroups == 0 {
            vec![Some(chars[s..e].iter().collect())]
        } else {
            (1..=ngroups).map(|g| caps[g].map(|(a, b)| chars[a..b].iter().collect())).collect()
        };
        rows.push(render_array(&elems));
        if !global {
            break;
        }
        from = if e > s { e } else { e + 1 };
        if from > chars.len() {
            break;
        }
    }
    Ok(rows)
}

/// `regexp_matches(text, pattern [, flags])` — one array of the
/// pattern's capture groups (or the whole match when it has none);
/// with `g`, every match's groups flattened; NULL / no-match → NULL
/// (the corpus never observes the set-returning row shape).
fn regexp_matches(args: &[Scalar]) -> Result<Scalar, ScalarError> {
    const FUNC: &str = "regexp_matches";
    if args.len() != 2 && args.len() != 3 {
        return Err(ScalarError::Arity { func: FUNC, got: args.len() });
    }
    if args.iter().any(Scalar::is_null) {
        // NULL input yields ZERO rows in PG (SRF), which the one-row
        // fold face cannot emit — refuse, don't fabricate a NULL row.
        return Err(ScalarError::Domain {
            func: FUNC,
            what: "NULL input yields no rows — set-returning, the fold face serves one row",
        });
    }
    let flags = match args.get(2) {
        None => "",
        Some(Scalar::Text(f)) => f.as_str(),
        Some(_) => return Err(ScalarError::Type { func: FUNC, arg: 2 }),
    };
    let (Some(t), Some(pat)) = (text(&args[0]), text(&args[1])) else {
        return Err(ScalarError::Type { func: FUNC, arg: 0 });
    };
    let node = compile(FUNC, pat, flags.contains('i'))?;
    let mut rows = match_rows(FUNC, &node, t, flags.contains('g'))?;
    // A set-returning function: the fold face serves exactly ONE row.
    // Zero rows (no match / NULL input — PG emits empty output) and
    // more than one (the g flag over several matches) are both out of
    // that model, so both refuse by name rather than answer wrong.
    match rows.len() {
        1 => Ok(Scalar::Text(rows.pop().expect("one row"))),
        n => Err(ScalarError::Domain {
            func: FUNC,
            what: if n == 0 {
                "no rows — regexp_matches is set-returning; the fold face serves one row"
            } else {
                "multiple rows with the g flag — the fold face serves one row"
            },
        }),
    }
}

/// `regexp_split_to_array(text, pattern [, flags])` — the pieces
/// between matches, as a `{...}` array.
fn regexp_split_to_array(args: &[Scalar]) -> Result<Scalar, ScalarError> {
    const FUNC: &str = "regexp_split_to_array";
    if args.len() != 2 && args.len() != 3 {
        return Err(ScalarError::Arity { func: FUNC, got: args.len() });
    }
    if args.iter().any(Scalar::is_null) {
        return Ok(Scalar::Null);
    }
    let flags = match args.get(2) {
        None => "",
        Some(Scalar::Text(f)) => f.as_str(),
        Some(_) => return Err(ScalarError::Type { func: FUNC, arg: 2 }),
    };
    let (Some(t), Some(pat)) = (text(&args[0]), text(&args[1])) else {
        return Err(ScalarError::Type { func: FUNC, arg: 0 });
    };
    let node = compile(FUNC, pat, flags.contains('i'))?;
    let chars: Vec<char> = t.chars().collect();
    let mut elems: Vec<Option<String>> = Vec::new();
    let (mut piece_start, mut from, err) = (0usize, 0usize, matcher_err(FUNC));
    loop {
        match re::re_find(&node, &chars, from).map_err(&err)? {
            Some((s, e)) => {
                elems.push(Some(chars[piece_start..s].iter().collect()));
                let step = if e > s { e } else { e + 1 };
                from = step;
                piece_start = step;
                if from > chars.len() {
                    break;
                }
            }
            None => {
                elems.push(Some(chars[piece_start..].iter().collect()));
                break;
            }
        }
    }
    Ok(Scalar::Text(render_array(&elems)))
}
