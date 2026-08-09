//! Boolean operators with PostgreSQL's three-valued logic, the
//! comparison operators, and the `::bool` text vocabulary — the operator
//! side of the scalar face (functions live in the sibling modules).

use crate::{Scalar, ScalarError};

/// Three-valued `AND`: `false` dominates, otherwise `NULL` if either
/// side is unknown (PG: `NULL AND false = false`).
pub fn logic_and(a: &Scalar, b: &Scalar) -> Result<Scalar, ScalarError> {
    match (as_bool3(a, "and", 0)?, as_bool3(b, "and", 1)?) {
        (Some(false), _) | (_, Some(false)) => Ok(Scalar::Bool(false)),
        (Some(true), Some(true)) => Ok(Scalar::Bool(true)),
        _ => Ok(Scalar::Null),
    }
}

/// Three-valued `OR`: `true` dominates (PG: `NULL OR true = true`).
pub fn logic_or(a: &Scalar, b: &Scalar) -> Result<Scalar, ScalarError> {
    match (as_bool3(a, "or", 0)?, as_bool3(b, "or", 1)?) {
        (Some(true), _) | (_, Some(true)) => Ok(Scalar::Bool(true)),
        (Some(false), Some(false)) => Ok(Scalar::Bool(false)),
        _ => Ok(Scalar::Null),
    }
}

/// Three-valued `NOT`: `NOT NULL` is `NULL`.
pub fn logic_not(a: &Scalar) -> Result<Scalar, ScalarError> {
    Ok(match as_bool3(a, "not", 0)? {
        Some(v) => Scalar::Bool(!v),
        None => Scalar::Null,
    })
}

fn as_bool3(v: &Scalar, func: &'static str, arg: usize) -> Result<Option<bool>, ScalarError> {
    match v {
        Scalar::Null => Ok(None),
        Scalar::Bool(b) => Ok(Some(*b)),
        _ => Err(ScalarError::Type { func, arg }),
    }
}

/// A comparison operator (`=`, `<>`, `<`, `<=`, `>`, `>=`) with strict
/// NULL propagation. Same-type comparisons only, plus the int↔float
/// numeric promotion; PG orders `false < true`, text compares bytewise
/// (C collation — the corpus subset's semantics).
pub fn cmp_op(op: &str, a: &Scalar, b: &Scalar) -> Result<Scalar, ScalarError> {
    use core::cmp::Ordering;
    let ord = match (a, b) {
        (Scalar::Null, _) | (_, Scalar::Null) => return Ok(Scalar::Null),
        (Scalar::Bool(x), Scalar::Bool(y)) => x.cmp(y),
        (Scalar::Int(x), Scalar::Int(y)) => x.cmp(y),
        (Scalar::Float(x), Scalar::Float(y)) => x
            .partial_cmp(y)
            .unwrap_or(Ordering::Equal),
        (Scalar::Int(x), Scalar::Float(y)) => (*x as f64)
            .partial_cmp(y)
            .unwrap_or(Ordering::Equal),
        (Scalar::Float(x), Scalar::Int(y)) => x
            .partial_cmp(&(*y as f64))
            .unwrap_or(Ordering::Equal),
        (Scalar::Text(x), Scalar::Text(y)) => x.as_bytes().cmp(y.as_bytes()),
        (Scalar::Timestamp(x), Scalar::Timestamp(y)) => x.cmp(y),
        (Scalar::Date(x), Scalar::Date(y)) => x.cmp(y),
        // A date compares against a timestamp at its midnight (PG's
        // date→timestamp promotion).
        (Scalar::Date(x), Scalar::Timestamp(y)) => (x * 86_400_000_000).cmp(y),
        (Scalar::Timestamp(x), Scalar::Date(y)) => x.cmp(&(y * 86_400_000_000)),
        _ => return Err(ScalarError::Type { func: "compare", arg: 1 }),
    };
    let hit = match op {
        "=" => ord == Ordering::Equal,
        "<>" | "!=" => ord != Ordering::Equal,
        "<" => ord == Ordering::Less,
        "<=" => ord != Ordering::Greater,
        ">" => ord == Ordering::Greater,
        ">=" => ord != Ordering::Less,
        _ => return Err(ScalarError::Type { func: "compare", arg: 0 }),
    };
    Ok(Scalar::Bool(hit))
}

/// PG's boolean input vocabulary (`boolin`): case-insensitive,
/// whitespace-trimmed `t/f true/false y/n yes/no on/off 1/0`.
/// Anything else is a cast error (the caller reports it by name).
pub fn parse_pg_bool(text: &str) -> Option<bool> {
    match text.trim().to_ascii_lowercase().as_str() {
        "t" | "true" | "y" | "yes" | "on" | "1" => Some(true),
        "f" | "false" | "n" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}
