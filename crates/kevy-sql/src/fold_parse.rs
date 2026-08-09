//! The fold grammar's primaries: literals, casts, `INTERVAL` and
//! clock keywords, function calls with their two SQL-flavored special
//! forms (`extract(field FROM x)`, `position(a IN b)`). Split from
//! `fold.rs` along the expression/primary seam (500-LOC discipline).

use crate::SqlError;
use crate::fold::Parser;
use crate::lex::Tok;
use kevy_scalar::Scalar;

const MICROS_PER_DAY: i64 = 86_400 * 1_000_000;

pub(crate) fn primary(p: &mut Parser<'_>) -> Result<Scalar, SqlError> {
    let t = p.bump();
    let (line, col) = (t.line, t.col);
    let v = match &t.tok {
        Tok::Num(text) => number(text, line, col)?,
        Tok::Str(s) => Scalar::Text(s.clone()),
        Tok::Sym('-') => {
            let inner = primary(p)?;
            kevy_scalar::binop('-', &Scalar::Int(0), &inner)
                .map_err(|e| SqlError::at(line, col, e.to_string()))?
        }
        Tok::Sym('(') => {
            let v = p.expr()?;
            p.expect_sym(')')?;
            v
        }
        Tok::Ident(w) => keyword_or_call(p, w, line, col)?,
        other => {
            return Err(SqlError::at(line, col, format!("expected an expression, got {other:?}")));
        }
    };
    cast_suffix(p, v)
}

/// `CASE WHEN cond THEN val [WHEN..THEN..]* [ELSE val] END` — the
/// searched form only (the corpus subset; the simple form's implicit
/// equality is a different construct and refuses by name). All arms
/// evaluate (fold is constant-only, there is nothing effectful to
/// short-circuit); a NULL / false condition just doesn't select.
fn case_expr(p: &mut Parser<'_>, line: u32, col: u32) -> Result<Scalar, SqlError> {
    let mut chosen: Option<Scalar> = None;
    let mut saw_when = false;
    while p.eat_kw("when") {
        saw_when = true;
        let cond = p.expr()?;
        p.expect_kw("then")?;
        let val = p.expr()?;
        if chosen.is_none() && matches!(cond, Scalar::Bool(true)) {
            chosen = Some(val);
        }
    }
    if !saw_when {
        return Err(SqlError::at(line, col, "CASE requires at least one WHEN"));
    }
    let else_val = if p.eat_kw("else") { Some(p.expr()?) } else { None };
    p.expect_kw("end")?;
    Ok(chosen.or(else_val).unwrap_or(Scalar::Null))
}

fn number(text: &str, line: u32, col: u32) -> Result<Scalar, SqlError> {
    if text.contains('.') {
        text.parse::<f64>()
            .map(Scalar::Float)
            .map_err(|_| SqlError::at(line, col, format!("bad numeric literal '{text}'")))
    } else {
        text.parse::<i64>()
            .map(Scalar::Int)
            .map_err(|_| SqlError::at(line, col, format!("bad integer literal '{text}'")))
    }
}

fn keyword_or_call(
    p: &mut Parser<'_>,
    word: &str,
    line: u32,
    col: u32,
) -> Result<Scalar, SqlError> {
    match word {
        "null" => return Ok(Scalar::Null),
        "true" => return Ok(Scalar::Bool(true)),
        "false" => return Ok(Scalar::Bool(false)),
        "case" => return case_expr(p, line, col),
        // The clock rewrites (probe 08/38): keyword forms have no
        // parens; now() below takes the call path.
        "current_timestamp" => return Ok(Scalar::Timestamp(p.now)),
        "current_date" => return Ok(Scalar::Date(p.now.div_euclid(MICROS_PER_DAY))),
        "interval" => {
            let t = p.bump();
            let Tok::Str(lit) = &t.tok else {
                return Err(SqlError::at(t.line, t.col, "INTERVAL needs a quoted literal"));
            };
            let Some((months, days, micros)) = kevy_scalar::parse_interval(lit) else {
                return Err(SqlError::at(t.line, t.col, format!("bad interval literal '{lit}'")));
            };
            return Ok(Scalar::Interval { months, days, micros });
        }
        _ => {}
    }
    if !p.eat_sym('(') {
        return Err(SqlError::at(
            line,
            col,
            format!(
                "column reference '{word}' is not foldable — only table-free \
                 SELECT folds here; a table query compiles through the plan face"
            ),
        ));
    }
    call(p, word, line, col)
}

/// A function call, after its `(`. The two SQL special forms rewrite
/// to plain argument lists here — kevy-scalar only ever sees
/// `(field, value)` / `(needle, haystack)`.
fn call(p: &mut Parser<'_>, func: &str, line: u32, col: u32) -> Result<Scalar, SqlError> {
    let mut args = Vec::new();
    if (func == "extract" || func == "date_part") && !matches!(p.peek().tok, Tok::Sym(')')) {
        // extract(FIELD FROM x) — the field word becomes the first arg.
        if func == "extract" {
            let t = p.bump();
            let Tok::Ident(field) = &t.tok else {
                return Err(SqlError::at(t.line, t.col, "extract needs a field name"));
            };
            args.push(Scalar::Text(field.clone()));
            p.expect_kw("from")?;
            args.push(p.expr()?);
            p.expect_sym(')')?;
            return finish(func, &args, line, col);
        }
    }
    if func == "now" {
        p.expect_sym(')')?;
        return Ok(Scalar::Timestamp(p.now));
    }
    if !matches!(p.peek().tok, Tok::Sym(')')) {
        loop {
            args.push(p.expr()?);
            // position(a IN b): the IN reads as an argument separator.
            if func == "position" && p.eat_kw("in") {
                args.push(p.expr()?);
                break;
            }
            if !p.eat_sym(',') {
                break;
            }
        }
    }
    p.expect_sym(')')?;
    finish(func, &args, line, col)
}

fn finish(func: &str, args: &[Scalar], line: u32, col: u32) -> Result<Scalar, SqlError> {
    kevy_scalar::eval(func, args).map_err(|e| SqlError::at(line, col, e.to_string()))
}

/// Zero or more `::type` suffixes. Casts are the corpus subset:
/// timestamp/date/interval parse their PG literal forms; int/text
/// convert; anything else is refused by name.
fn cast_suffix(p: &mut Parser<'_>, mut v: Scalar) -> Result<Scalar, SqlError> {
    loop {
        if !matches!(p.peek().tok, Tok::Op("::")) {
            return Ok(v);
        }
        p.bump();
        let t = p.bump();
        let Tok::Ident(ty) = &t.tok else {
            return Err(SqlError::at(t.line, t.col, "expected a type name after '::'"));
        };
        v = cast(&v, ty).map_err(|why| SqlError::at(t.line, t.col, why))?;
    }
}


/// The datetime text-literal casts, split from [`cast`] (50-LOC rule).
fn cast_datetime(v: &Scalar, ty: &str) -> Result<Scalar, String> {
    // The caller matched Text; by argument any other variant reaching
    // here is a wiring bug — answer the honest cast error, not a panic.
    let Scalar::Text(s) = v else {
        return Err(format!("cannot cast this value to {ty}"));
    };
    Ok(match ty {
        "timestamp" | "datetime" => Scalar::Timestamp(
            kevy_scalar::parse_timestamp(s)
                .ok_or_else(|| format!("bad timestamp literal '{s}'"))?,
        ),
        "date" => Scalar::Date(
            kevy_scalar::parse_date(s).ok_or_else(|| format!("bad date literal '{s}'"))?,
        ),
        _ => {
            let (months, days, micros) = kevy_scalar::parse_interval(s)
                .ok_or_else(|| format!("bad interval literal '{s}'"))?;
            Scalar::Interval { months, days, micros }
        }
    })
}

fn cast(v: &Scalar, ty: &str) -> Result<Scalar, String> {
    if let ("timestamp" | "datetime" | "date" | "interval", Scalar::Text(_)) = (ty, v) {
        return cast_datetime(v, ty);
    }
    let out = match (ty, v) {
        (_, Scalar::Null) => Scalar::Null,
        ("bool" | "boolean", Scalar::Text(s)) => Scalar::Bool(
            kevy_scalar::parse_pg_bool(s)
                .ok_or_else(|| format!("invalid input syntax for type boolean: '{s}'"))?,
        ),
        ("bool" | "boolean", Scalar::Bool(b)) => Scalar::Bool(*b),
        // PG: int → bool is 0 = false, non-zero = true.
        ("bool" | "boolean", Scalar::Int(i)) => Scalar::Bool(*i != 0),
        ("int" | "integer" | "bigint" | "int4" | "int8", Scalar::Bool(b)) => {
            Scalar::Int(i64::from(*b))
        }
        ("int" | "integer" | "bigint" | "int4" | "int8", Scalar::Text(s)) => Scalar::Int(
            s.trim().parse().map_err(|_| format!("'{s}' is not an integer"))?,
        ),
        ("int" | "integer" | "bigint" | "int4" | "int8", Scalar::Int(i)) => Scalar::Int(*i),
        ("int" | "integer" | "bigint" | "int4" | "int8", Scalar::Float(f)) => {
            // PG rounds (half away) on float→int casts, not truncates.
            Scalar::Int(if *f >= 0.0 { (f + 0.5).floor() } else { (f - 0.5).ceil() } as i64)
        }
        // PG's bool→text cast spells the word out ('true'/'false');
        // the terse 't'/'f' is the wire form, not the cast result.
        ("text" | "varchar", Scalar::Bool(b)) => {
            Scalar::Text(if *b { "true" } else { "false" }.into())
        }
        ("text" | "varchar", other) => Scalar::Text(other.render()),
        ("float" | "float8" | "double" | "numeric" | "decimal", Scalar::Int(i)) => {
            Scalar::Float(*i as f64)
        }
        ("float" | "float8" | "double" | "numeric" | "decimal", Scalar::Float(f)) => {
            Scalar::Float(*f)
        }
        ("float" | "float8" | "double" | "numeric" | "decimal", Scalar::Text(s)) => Scalar::Float(
            s.trim().parse().map_err(|_| format!("'{s}' is not a number"))?,
        ),
        _ => return Err(format!("cast to '{ty}' is not supported here")),
    };
    Ok(out)
}
