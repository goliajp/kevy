//! The NULL family — the four functions whose whole point is that
//! they do NOT propagate NULL strictly: `coalesce` answers the first
//! non-NULL, `greatest`/`least` silently skip NULLs (probe 51), and
//! `nullif` manufactures one.

use crate::{Scalar, ScalarError};
use std::cmp::Ordering;

pub(crate) fn eval(name: &str, args: &[Scalar]) -> Result<Scalar, ScalarError> {
    match name {
        "coalesce" => Ok(args.iter().find(|a| !a.is_null()).cloned().unwrap_or(Scalar::Null)),
        "nullif" => nullif(args),
        "greatest" => extreme("greatest", args, Ordering::Greater),
        "least" => extreme("least", args, Ordering::Less),
        _ => Err(ScalarError::UnknownFunction(name.to_string())),
    }
}

/// `nullif(a, b)` — NULL when the two compare equal, else `a`. A NULL
/// `b` never equals anything, so `a` comes back unchanged.
fn nullif(args: &[Scalar]) -> Result<Scalar, ScalarError> {
    let [a, b] = args else {
        return Err(ScalarError::Arity { func: "nullif", got: args.len() });
    };
    if a.is_null() {
        return Ok(Scalar::Null);
    }
    match compare("nullif", a, b) {
        Ok(Some(Ordering::Equal)) => Ok(Scalar::Null),
        Ok(_) => Ok(a.clone()),
        Err(e) => Err(e),
    }
}

/// Variadic max/min: NULLs skipped, all-NULL answers NULL, numerics
/// widen across Int/Float, text compares lexicographically. Mixing
/// text with numbers is a type error (PG refuses the cast).
fn extreme(func: &'static str, args: &[Scalar], keep: Ordering) -> Result<Scalar, ScalarError> {
    if args.is_empty() {
        return Err(ScalarError::Arity { func, got: 0 });
    }
    let mut best: Option<&Scalar> = None;
    for a in args {
        if a.is_null() {
            continue;
        }
        best = Some(match best {
            None => a,
            Some(b) => match compare(func, a, b)? {
                Some(ord) if ord == keep => a,
                Some(_) => b,
                None => b,
            },
        });
    }
    Ok(best.cloned().unwrap_or(Scalar::Null))
}

/// Cross-type comparison for the family: `None` when either side is
/// NULL (SQL: unknown), `Err` when the types are incomparable.
fn compare(
    func: &'static str,
    a: &Scalar,
    b: &Scalar,
) -> Result<Option<Ordering>, ScalarError> {
    let ord = match (a, b) {
        (Scalar::Null, _) | (_, Scalar::Null) => return Ok(None),
        (Scalar::Int(x), Scalar::Int(y)) => x.cmp(y),
        (Scalar::Text(x), Scalar::Text(y)) => x.cmp(y),
        (Scalar::Bool(x), Scalar::Bool(y)) => x.cmp(y),
        (Scalar::Int(_) | Scalar::Float(_), Scalar::Int(_) | Scalar::Float(_)) => {
            let (x, y) = (as_f64(a), as_f64(b));
            x.partial_cmp(&y).ok_or(ScalarError::Domain {
                func,
                what: "NaN is not orderable here",
            })?
        }
        _ => return Err(ScalarError::Type { func, arg: 1 }),
    };
    Ok(Some(ord))
}

fn as_f64(v: &Scalar) -> f64 {
    match v {
        Scalar::Int(i) => *i as f64,
        Scalar::Float(f) => *f,
        _ => f64::NAN,
    }
}
