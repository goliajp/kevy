//! Binary operators over [`Scalar`] — the `+ - * /` the sql face's
//! constant folder delegates here so operator semantics live with the
//! rest of the library. Calendar arithmetic follows probe 10: the
//! month half of an interval moves via kevy-time's clamped
//! `add_months`, the micros half is exact.

use crate::datetime::{MICROS_PER_DAY, MICROS_PER_SEC};
use crate::{Scalar, ScalarError};

/// Apply `a <op> b` where `op` is one of `+ - * /`.
///
/// # Examples
///
/// ```
/// use kevy_scalar::{Scalar, binop};
/// assert_eq!(binop('+', &Scalar::Int(2), &Scalar::Int(3)).unwrap(), Scalar::Int(5));
/// // Strict in NULL, like the function library.
/// assert_eq!(binop('*', &Scalar::Int(2), &Scalar::Null).unwrap(), Scalar::Null);
/// ```
// LOC-WAIVER: a pure type-dispatch match table — one arm per
// (operator, operand-type) pair, no control flow beyond the match.
pub fn binop(op: char, a: &Scalar, b: &Scalar) -> Result<Scalar, ScalarError> {
    use Scalar as S;
    if a.is_null() || b.is_null() {
        return Ok(S::Null);
    }
    match (op, a, b) {
        // ── numbers ──
        ('+', S::Int(x), S::Int(y)) => checked(x.checked_add(*y)),
        ('-', S::Int(x), S::Int(y)) => checked(x.checked_sub(*y)),
        ('*', S::Int(x), S::Int(y)) => checked(x.checked_mul(*y)),
        ('/', S::Int(_), S::Int(0)) => {
            Err(ScalarError::Domain { func: "/", what: "division by zero" })
        }
        // PG integer division truncates toward zero.
        ('/', S::Int(x), S::Int(y)) => Ok(S::Int(x.wrapping_div(*y))),
        (_, S::Int(_) | S::Float(_), S::Int(_) | S::Float(_)) => float_op(op, a, b),
        // ── timestamp/date ± interval (both operand orders for +) ──
        ('+', S::Timestamp(us), S::Interval { months, days, micros })
        | ('+', S::Interval { months, days, micros }, S::Timestamp(us)) => {
            Ok(S::Timestamp(shift(*us, *months, days * MICROS_PER_DAY + micros)))
        }
        ('-', S::Timestamp(us), S::Interval { months, days, micros }) => {
            Ok(S::Timestamp(shift(*us, -months, -(days * MICROS_PER_DAY + micros))))
        }
        ('+', S::Date(d), S::Interval { months, days, micros })
        | ('+', S::Interval { months, days, micros }, S::Date(d)) => {
            Ok(S::Timestamp(shift(d * MICROS_PER_DAY, *months, days * MICROS_PER_DAY + micros)))
        }
        ('-', S::Date(d), S::Interval { months, days, micros }) => {
            Ok(S::Timestamp(shift(d * MICROS_PER_DAY, -months, -(days * MICROS_PER_DAY + micros))))
        }
        // date ± int = date (whole days, PG rule).
        ('+', S::Date(d), S::Int(n)) | ('+', S::Int(n), S::Date(d)) => Ok(S::Date(d + n)),
        ('-', S::Date(d), S::Int(n)) => Ok(S::Date(d - n)),
        // ── differences ──
        // PG pulls whole days out of a timestamp difference ("2 days",
        // not "48:00:00").
        ('-', S::Timestamp(a), S::Timestamp(b)) => {
            let diff = a - b;
            Ok(S::Interval {
                months: 0,
                days: diff / MICROS_PER_DAY,
                micros: diff % MICROS_PER_DAY,
            })
        }
        ('-', S::Date(a), S::Date(b)) => Ok(S::Int(a - b)), // days, an integer in PG
        // ── interval ± interval ──
        (
            '+',
            S::Interval { months: m1, days: d1, micros: u1 },
            S::Interval { months: m2, days: d2, micros: u2 },
        ) => Ok(S::Interval { months: m1 + m2, days: d1 + d2, micros: u1 + u2 }),
        (
            '-',
            S::Interval { months: m1, days: d1, micros: u1 },
            S::Interval { months: m2, days: d2, micros: u2 },
        ) => Ok(S::Interval { months: m1 - m2, days: d1 - d2, micros: u1 - u2 }),
        _ => Err(ScalarError::Domain { func: "operator", what: "operand types not supported" }),
    }
}

fn checked(v: Option<i64>) -> Result<Scalar, ScalarError> {
    v.map(Scalar::Int)
        .ok_or(ScalarError::Domain { func: "operator", what: "bigint out of range" })
}

fn float_op(op: char, a: &Scalar, b: &Scalar) -> Result<Scalar, ScalarError> {
    let f = |v: &Scalar| match v {
        Scalar::Int(i) => *i as f64,
        Scalar::Float(x) => *x,
        _ => f64::NAN,
    };
    let (x, y) = (f(a), f(b));
    let out = match op {
        '+' => x + y,
        '-' => x - y,
        '*' => x * y,
        '/' => {
            if y == 0.0 {
                return Err(ScalarError::Domain { func: "/", what: "division by zero" });
            }
            x / y
        }
        _ => {
            return Err(ScalarError::Domain { func: "operator", what: "unknown operator" });
        }
    };
    Ok(Scalar::Float(out))
}

/// Move a timestamp by an interval: months through the clamped
/// calendar walk, micros exactly.
fn shift(us: i64, months: i64, micros: i64) -> i64 {
    let secs = us.div_euclid(MICROS_PER_SEC);
    let frac = us.rem_euclid(MICROS_PER_SEC);
    let moved = if months != 0 { kevy_time::add_months(secs, months) } else { secs };
    moved * MICROS_PER_SEC + frac + micros
}
