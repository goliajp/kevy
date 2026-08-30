//! Math functions. The three rounding rules PG distinguishes (probe
//! 46-49) are the whole reason this module exists as more than libm
//! calls: `floor` goes toward −infinity, `trunc` toward zero, `round`
//! half-AWAY-from-zero (numeric semantics — not banker's rounding).
//! Integer inputs pass through the integer path unchanged.

use crate::{Scalar, ScalarError};

pub(crate) fn eval(name: &str, args: &[Scalar]) -> Result<Scalar, ScalarError> {
    match name {
        "floor" => unary("floor", args, f64::floor),
        "ceil" | "ceiling" => unary("ceil", args, f64::ceil),
        "trunc" => scaled("trunc", args, trunc_scaled),
        "round" => scaled("round", args, round_scaled),
        "abs" => abs(args),
        "sign" => sign(args),
        "sqrt" => sqrt(args),
        "mod" => modulo(args),
        "power" | "pow" => power(args),
        _ => Err(ScalarError::UnknownFunction(name.to_string())),
    }
}

/// PG's text output form for a float result: integral values render
/// without a fraction (`floor(1.7)` prints `1`), everything else in
/// shortest-roundtrip form.
pub(crate) fn render_float(f: f64) -> String {
    if f.fract() == 0.0 && f.abs() < 1e15 { format!("{}", f as i64) } else { format!("{f}") }
}

/// Int passthrough, Float mapped; strict NULL.
fn unary(
    func: &'static str,
    args: &[Scalar],
    f: impl Fn(f64) -> f64,
) -> Result<Scalar, ScalarError> {
    match args {
        [Scalar::Null] => Ok(Scalar::Null),
        [Scalar::Int(i)] => Ok(Scalar::Int(*i)),
        [Scalar::Float(x)] => Ok(Scalar::Float(f(*x))),
        [_] => Err(ScalarError::Type { func, arg: 0 }),
        _ => Err(ScalarError::Arity { func, got: args.len() }),
    }
}

/// `round`/`trunc` share the one- and two-argument shapes: the second
/// argument is the decimal scale (negative = tens/hundreds/…).
fn scaled(
    func: &'static str,
    args: &[Scalar],
    f: impl Fn(f64, i64) -> f64,
) -> Result<Scalar, ScalarError> {
    match args {
        [Scalar::Null] | [Scalar::Null, _] | [_, Scalar::Null] => Ok(Scalar::Null),
        [Scalar::Int(i)] => Ok(Scalar::Int(*i)),
        [Scalar::Float(x)] => Ok(Scalar::Float(f(*x, 0))),
        [Scalar::Int(i), Scalar::Int(s)] => {
            if *s >= 0 {
                return Ok(Scalar::Int(*i));
            }
            Ok(Scalar::Float(f(*i as f64, *s)))
        }
        [Scalar::Float(x), Scalar::Int(s)] => Ok(Scalar::Float(f(*x, *s))),
        [_] | [_, _] => Err(ScalarError::Type { func, arg: 0 }),
        _ => Err(ScalarError::Arity { func, got: args.len() }),
    }
}

/// Half-away-from-zero at `scale` decimal places.
fn round_scaled(x: f64, scale: i64) -> f64 {
    let factor = 10f64.powi(scale.clamp(-18, 18) as i32);
    let shifted = x * factor;
    let rounded = if shifted >= 0.0 { (shifted + 0.5).floor() } else { (shifted - 0.5).ceil() };
    rounded / factor
}

/// Toward zero at `scale` decimal places.
fn trunc_scaled(x: f64, scale: i64) -> f64 {
    let factor = 10f64.powi(scale.clamp(-18, 18) as i32);
    (x * factor).trunc() / factor
}

fn abs(args: &[Scalar]) -> Result<Scalar, ScalarError> {
    match args {
        [Scalar::Null] => Ok(Scalar::Null),
        [Scalar::Int(i)] => i
            .checked_abs()
            .map(Scalar::Int)
            .ok_or(ScalarError::Domain { func: "abs", what: "bigint out of range" }),
        [Scalar::Float(x)] => Ok(Scalar::Float(x.abs())),
        [_] => Err(ScalarError::Type { func: "abs", arg: 0 }),
        _ => Err(ScalarError::Arity { func: "abs", got: args.len() }),
    }
}

fn sign(args: &[Scalar]) -> Result<Scalar, ScalarError> {
    match args {
        [Scalar::Null] => Ok(Scalar::Null),
        [Scalar::Int(i)] => Ok(Scalar::Int(i.signum())),
        [Scalar::Float(x)] => Ok(Scalar::Int(if *x > 0.0 {
            1
        } else if *x < 0.0 {
            -1
        } else {
            0
        })),
        [_] => Err(ScalarError::Type { func: "sign", arg: 0 }),
        _ => Err(ScalarError::Arity { func: "sign", got: args.len() }),
    }
}

fn sqrt(args: &[Scalar]) -> Result<Scalar, ScalarError> {
    let x = match args {
        [Scalar::Null] => return Ok(Scalar::Null),
        [Scalar::Int(i)] => *i as f64,
        [Scalar::Float(x)] => *x,
        [_] => return Err(ScalarError::Type { func: "sqrt", arg: 0 }),
        _ => return Err(ScalarError::Arity { func: "sqrt", got: args.len() }),
    };
    if x < 0.0 {
        return Err(ScalarError::Domain {
            func: "sqrt",
            what: "cannot take square root of a negative number",
        });
    }
    Ok(Scalar::Float(x.sqrt()))
}

/// Integer modulo; the result's sign follows the DIVIDEND (probe 52 —
/// Rust's `%` agrees with PG here).
fn modulo(args: &[Scalar]) -> Result<Scalar, ScalarError> {
    match args {
        [Scalar::Null, _] | [_, Scalar::Null] => Ok(Scalar::Null),
        [Scalar::Int(_), Scalar::Int(0)] => {
            Err(ScalarError::Domain { func: "mod", what: "division by zero" })
        }
        [Scalar::Int(a), Scalar::Int(b)] => Ok(Scalar::Int(a.wrapping_rem(*b))),
        [_, _] => Err(ScalarError::Type { func: "mod", arg: 0 }),
        _ => Err(ScalarError::Arity { func: "mod", got: args.len() }),
    }
}

/// Integer base with a non-negative integer exponent stays exact;
/// anything else goes through `powf`. PG errors on the two complex /
/// undefined corners (probe 53).
fn power(args: &[Scalar]) -> Result<Scalar, ScalarError> {
    let (x, y, both_int) = match args {
        [Scalar::Null, _] | [_, Scalar::Null] => return Ok(Scalar::Null),
        [Scalar::Int(a), Scalar::Int(b)] => (*a as f64, *b as f64, Some((*a, *b))),
        [Scalar::Int(a), Scalar::Float(b)] => (*a as f64, *b, None),
        [Scalar::Float(a), Scalar::Int(b)] => (*a, *b as f64, None),
        [Scalar::Float(a), Scalar::Float(b)] => (*a, *b, None),
        [_, _] => return Err(ScalarError::Type { func: "power", arg: 0 }),
        _ => return Err(ScalarError::Arity { func: "power", got: args.len() }),
    };
    if x == 0.0 && y < 0.0 {
        return Err(ScalarError::Domain {
            func: "power",
            what: "zero raised to a negative power is undefined",
        });
    }
    if x < 0.0 && y.fract() != 0.0 {
        return Err(ScalarError::Domain {
            func: "power",
            what: "a negative number raised to a non-integer power yields a complex result",
        });
    }
    if let Some((a, b)) = both_int
        && b >= 0
    {
        let exp = u32::try_from(b)
            .ok()
            .ok_or(ScalarError::Domain { func: "power", what: "bigint out of range" })?;
        return a
            .checked_pow(exp)
            .map(Scalar::Int)
            .ok_or(ScalarError::Domain { func: "power", what: "bigint out of range" });
    }
    let r = x.powf(y);
    // PG's numeric power renders fractional-exponent results at scale
    // 16 ("2.0000000000000000", probe 53) while integer exponents keep
    // the short form (0.125). f64's shortest form already matches the
    // irrational cases; the integral-valued ones need the padding, and
    // Text is the honest carrier — arithmetic on it fails loud (a Type
    // refusal), never silently mis-renders.
    if y.fract() != 0.0 {
        return Ok(Scalar::Text(format!("{r:.16}")));
    }
    Ok(Scalar::Float(r))
}
