//! kevy-scalar — PG-canonical scalar function evaluation.
//!
//! The function library behind kevy's sql face (V1 train, RFC
//! 2026-08-08): `SELECT lower('X')`-shaped constant folding and the
//! query-card projection epilogue both call [`eval`]. Nothing here
//! ever runs inside a serving engine process — evaluation stays in
//! the sql face by design (Law 3), which is why this crate knows
//! nothing about kevy: it maps a function name and [`Scalar`]
//! arguments to a [`Scalar`] result, PG 18 semantics, and that is the
//! whole contract.
//!
//! Semantics ground truth: the pg_regress-derived probe corpus
//! (`bench/funcgate-corpus/`). The tests in this crate are transcribed
//! from those files — where PG is surprising (floor toward −infinity,
//! `trim` strips a character SET not a substring, NULL propagates
//! through almost everything), the probe line is cited.
//!
//! ```
//! use kevy_scalar::{eval, Scalar};
//! let out = eval("lower", &[Scalar::Text("HeLLo".into())]).unwrap();
//! assert_eq!(out, Scalar::Text("hello".into()));
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod datetime;
mod datetime_fmt;
mod math;
mod nullfam;
mod ops;
mod strings;
mod strings_slice;
#[cfg(test)]
mod tests;

pub use ops::binop;

pub use datetime_fmt::{
    parse_date, parse_interval, parse_timestamp, render_date, render_interval,
    render_timestamp,
};

/// A typed scalar value — the closed set the function library speaks.
///
/// Deliberately narrower than SQL's type zoo: kevy's sql face maps
/// bigint/int → `Int`, double/numeric-literal → `Float`, text/varchar
/// → `Text`, boolean → `Bool`, and SQL `NULL` → `Null`. Types the
/// engine refuses (money, inet, enum, …) never reach this crate.
#[derive(Debug, Clone, PartialEq)]
pub enum Scalar {
    /// SQL NULL. Propagates: any strict function with a NULL argument
    /// answers NULL (the null-family module documents its exceptions).
    Null,
    /// A 64-bit integer.
    Int(i64),
    /// A 64-bit float. PG's numeric literals fold through here; the
    /// sql face renders results back with PG's trailing-zero rules.
    Float(f64),
    /// A UTF-8 string.
    Text(String),
    /// A boolean.
    Bool(bool),
    /// A timestamp (no time zone): microseconds since the Unix epoch.
    /// Probe 38's lesson is baked in: `now()` and friends must be
    /// TYPED, not text — the sql face rewrites them to this variant.
    Timestamp(i64),
    /// A calendar date: days since the Unix epoch.
    Date(i64),
    /// An interval in PG's three-component shape: months, days and
    /// microseconds never mix (probe 10's `1 day - 12 hours` stays
    /// `1 day -12:00:00` — no normalization across components). Month
    /// arithmetic clamps to month ends; the other two are exact.
    Interval {
        /// Whole calendar months (12 per year).
        months: i64,
        /// Whole days — separate from micros because PG keeps them
        /// separate (visible in rendering and component extraction).
        days: i64,
        /// Sub-day remainder in microseconds.
        micros: i64,
    },
}

impl Scalar {
    /// Whether this is SQL NULL.
    #[must_use]
    pub fn is_null(&self) -> bool {
        matches!(self, Scalar::Null)
    }

    /// PG's text output form for this value. `Null` renders as the
    /// empty string here — a caller that needs a `NULL` marker (the
    /// sqllogictest runner does) checks [`Scalar::is_null`] first.
    #[must_use]
    pub fn render(&self) -> String {
        strings::to_text(self)
    }
}

/// Why a call could not be evaluated.
///
/// `UnknownFunction` is the load-bearing variant: the sql face turns
/// it into a *named refusal* (the funcgate contract says silent
/// failure is itself a gate failure), so the message must carry the
/// function name verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScalarError {
    /// No function with this name in the library. Field = the name.
    UnknownFunction(String),
    /// The function exists but not with this argument count.
    Arity {
        /// Function name.
        func: &'static str,
        /// What the call supplied.
        got: usize,
    },
    /// An argument had a type the function cannot take (PG would have
    /// refused the cast at parse time; the sql face reports this with
    /// the same vocabulary).
    Type {
        /// Function name.
        func: &'static str,
        /// 0-based argument position.
        arg: usize,
    },
    /// Arithmetic that PG raises on: division by zero, sqrt of a
    /// negative, integer overflow.
    Domain {
        /// Function name.
        func: &'static str,
        /// PG's error phrase, e.g. `division by zero`.
        what: &'static str,
    },
}

impl std::fmt::Display for ScalarError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScalarError::UnknownFunction(name) => {
                write!(f, "unknown function: {name}")
            }
            ScalarError::Arity { func, got } => {
                write!(f, "{func}: wrong argument count ({got})")
            }
            ScalarError::Type { func, arg } => {
                write!(f, "{func}: argument {n} has an unsupported type", n = arg + 1)
            }
            ScalarError::Domain { func, what } => write!(f, "{func}: {what}"),
        }
    }
}

impl std::error::Error for ScalarError {}

/// Evaluate `func(args…)` with PG 18 semantics.
///
/// Function names are matched case-insensitively (PG folds unquoted
/// identifiers). Strict NULL propagation is handled per-function in
/// the modules: most functions answer `Null` when any argument is
/// `Null`; `coalesce`/`greatest`/`least` and friends look through.
pub fn eval(func: &str, args: &[Scalar]) -> Result<Scalar, ScalarError> {
    let name = func.to_ascii_lowercase();
    match name.as_str() {
        // ── strings ──
        "lower" | "upper" | "initcap" | "length" | "char_length" | "character_length"
        | "concat" | "concat_ws" | "trim" | "btrim" | "ltrim" | "rtrim" | "replace"
        | "split_part" | "repeat" | "lpad" | "rpad" | "strpos" | "position" | "left"
        | "right" | "reverse" | "translate" | "substr" | "substring" => {
            strings::eval(&name, args)
        }
        // ── math ──
        "floor" | "ceil" | "ceiling" | "round" | "trunc" | "mod" | "power" | "pow"
        | "sqrt" | "sign" | "abs" => math::eval(&name, args),
        // ── null family ──
        "coalesce" | "nullif" | "greatest" | "least" => nullfam::eval(&name, args),
        // ── date/time ──
        "extract" | "date_part" | "date_trunc" | "age" | "to_char" => {
            datetime::eval(&name, args)
        }
        _ => Err(ScalarError::UnknownFunction(func.to_string())),
    }
}
