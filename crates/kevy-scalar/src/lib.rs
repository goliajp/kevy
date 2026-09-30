//! kevy-scalar — PG-canonical scalar function evaluation.
//!
//! The function library behind kevy's sql face (the V1 scalar
//! surface): `SELECT lower('X')`-shaped constant folding and the
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
//! let out = eval("lower", &[Scalar::Text("HeLLo".into())])?;
//! assert_eq!(out, Scalar::Text("hello".into()));
//! # Ok::<(), kevy_scalar::ScalarError>(())
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod datetime;
mod datetime_fmt;
mod logic;
mod math;
mod md5;
mod nullfam;
mod ops;
mod regex_engine;
mod regexp;
mod strings;
mod strings_slice;
#[cfg(test)]
mod tests;

pub use logic::{cmp_op, logic_and, logic_not, logic_or, parse_pg_bool};
pub use ops::binop;

pub use datetime_fmt::{
    parse_date, parse_interval, parse_timestamp, render_date, render_interval, render_timestamp,
};

// Send and Sync are part of the public contract: a change that loses
// either fails to compile here rather than in a caller.
const _: () = {
    const fn send_sync<T: Send + Sync>() {}
    send_sync::<Scalar>();
    send_sync::<ScalarError>();
};

/// A typed scalar value — the closed set the function library speaks.
///
/// Deliberately narrower than SQL's type zoo: kevy's sql face maps
/// bigint/int → `Int`, double/numeric-literal → `Float`, text/varchar
/// → `Text`, boolean → `Bool`, and SQL `NULL` → `Null`. Types the
/// engine refuses (money, inet, enum, …) never reach this crate.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
/// # Examples
///
/// ```
/// use kevy_scalar::{Scalar, eval};
/// // NULL is a value here, not an absence, and it PROPAGATES: a strict
/// // function with a NULL argument answers NULL rather than erroring.
/// assert_eq!(eval("upper", &[Scalar::Null]).unwrap(), Scalar::Null);
/// assert_eq!(
///     eval("upper", &[Scalar::Text("ada".into())]).unwrap(),
///     Scalar::Text("ADA".into())
/// );
/// ```
pub enum Scalar {
    /// SQL NULL. Propagates: any strict function with a NULL argument
    /// answers NULL (the null-family module documents its exceptions).
    ///
    /// ```
    /// use kevy_scalar::{Scalar, eval};
    /// assert_eq!(eval("lower", &[Scalar::Null])?, Scalar::Null);
    /// assert_eq!(eval("coalesce", &[Scalar::Null, Scalar::Int(1)])?, Scalar::Int(1));
    /// # Ok::<(), kevy_scalar::ScalarError>(())
    /// ```
    Null,
    /// A 64-bit integer.
    ///
    /// ```
    /// use kevy_scalar::{Scalar, eval};
    /// assert_eq!(eval("length", &[Scalar::Text("kevy".into())])?, Scalar::Int(4));
    /// assert_eq!(Scalar::Int(-7).render(), "-7");
    /// # Ok::<(), kevy_scalar::ScalarError>(())
    /// ```
    Int(i64),
    /// A 64-bit float. PG's numeric literals fold through here; the
    /// sql face renders results back with PG's trailing-zero rules.
    ///
    /// ```
    /// use kevy_scalar::{Scalar, binop};
    /// let half = binop('/', &Scalar::Float(1.0), &Scalar::Int(2))?;
    /// assert_eq!(half, Scalar::Float(0.5));
    /// assert_eq!(half.render(), "0.5");
    /// # Ok::<(), kevy_scalar::ScalarError>(())
    /// ```
    Float(f64),
    /// A UTF-8 string.
    ///
    /// ```
    /// use kevy_scalar::{Scalar, eval};
    /// let out = eval("initcap", &[Scalar::Text("hello world".into())])?;
    /// assert_eq!(out, Scalar::Text("Hello World".into()));
    /// # Ok::<(), kevy_scalar::ScalarError>(())
    /// ```
    Text(String),
    /// A boolean.
    ///
    /// ```
    /// use kevy_scalar::{Scalar, logic_not};
    /// assert_eq!(logic_not(&Scalar::Bool(true))?, Scalar::Bool(false));
    /// assert_eq!(Scalar::Bool(false).render(), "f");
    /// # Ok::<(), kevy_scalar::ScalarError>(())
    /// ```
    Bool(bool),
    /// A timestamp (no time zone): microseconds since the Unix epoch.
    /// Probe 38's lesson is baked in: `now()` and friends must be
    /// TYPED, not text — the sql face rewrites them to this variant.
    ///
    /// ```
    /// use kevy_scalar::{Scalar, parse_timestamp};
    /// let us = parse_timestamp("1970-01-02 00:00:00").unwrap();
    /// assert_eq!(us, 86_400_000_000);
    /// assert_eq!(Scalar::Timestamp(us).render(), "1970-01-02 00:00:00");
    /// ```
    Timestamp(i64),
    /// A calendar date: days since the Unix epoch.
    ///
    /// ```
    /// use kevy_scalar::{Scalar, binop, parse_date};
    /// let d = Scalar::Date(parse_date("2024-02-28").unwrap());
    /// let next = binop('+', &d, &Scalar::Int(1))?;
    /// assert_eq!(next.render(), "2024-02-29");
    /// # Ok::<(), kevy_scalar::ScalarError>(())
    /// ```
    Date(i64),
    /// An interval in PG's three-component shape: months, days and
    /// microseconds never mix (probe 10's `1 day - 12 hours` stays
    /// `1 day -12:00:00` — no normalization across components). Month
    /// arithmetic clamps to month ends; the other two are exact.
    ///
    /// ```
    /// use kevy_scalar::{Scalar, binop, parse_date};
    /// let jan31 = Scalar::Date(parse_date("2024-01-31").unwrap());
    /// let month = Scalar::Interval { months: 1, days: 0, micros: 0 };
    /// // clamped to the last day of February
    /// assert_eq!(binop('+', &jan31, &month)?.render(), "2024-02-29 00:00:00");
    /// # Ok::<(), kevy_scalar::ScalarError>(())
    /// ```
    Interval {
        /// Whole calendar months (12 per year).
        ///
        /// ```
        /// use kevy_scalar::{Scalar, parse_interval};
        /// let (months, _, _) = parse_interval("1 year 2 mons").unwrap();
        /// assert_eq!(months, 14);
        /// let iv = Scalar::Interval { months, days: 0, micros: 0 };
        /// assert_eq!(iv.render(), "1 year 2 mons");
        /// ```
        months: i64,
        /// Whole days — separate from micros because PG keeps them
        /// separate (visible in rendering and component extraction).
        ///
        /// ```
        /// use kevy_scalar::Scalar;
        /// // 1 day minus 12 hours stays as two components
        /// let iv = Scalar::Interval { months: 0, days: 1, micros: -43_200_000_000 };
        /// assert_eq!(iv.render(), "1 day -12:00:00");
        /// ```
        days: i64,
        /// Sub-day remainder in microseconds.
        ///
        /// ```
        /// use kevy_scalar::Scalar;
        /// let iv = Scalar::Interval { months: 0, days: 0, micros: 5_400_000_000 };
        /// assert_eq!(iv.render(), "01:30:00");
        /// ```
        micros: i64,
    },
}

impl Scalar {
    /// Whether this is SQL NULL.
    #[must_use]
    /// # Examples
    ///
    /// ```
    /// use kevy_scalar::Scalar;
    /// assert!(Scalar::Null.is_null());
    /// assert!(!Scalar::Int(0).is_null(), "zero is a value, not a NULL");
    /// assert!(!Scalar::Text(String::new()).is_null(), "so is the empty string");
    /// ```
    pub fn is_null(&self) -> bool {
        matches!(self, Scalar::Null)
    }

    /// PG's text output form for this value. `Null` renders as the
    /// empty string here — a caller that needs a `NULL` marker (the
    /// sqllogictest runner does) checks [`Scalar::is_null`] first.
    #[must_use]
    /// # Examples
    ///
    /// ```
    /// use kevy_scalar::Scalar;
    /// assert_eq!(Scalar::Int(42).render(), "42");
    /// assert_eq!(Scalar::Text("ada".into()).render(), "ada");
    /// assert_eq!(Scalar::Bool(true).render(), "t", "PG's boolean output");
    /// ```
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
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
/// # Examples
///
/// ```
/// use kevy_scalar::{Scalar, ScalarError, eval};
/// // A name this engine does not implement is refused BY NAME rather
/// // than silently answering NULL.
/// assert!(matches!(
///     eval("no_such_fn", &[]),
///     Err(ScalarError::UnknownFunction(_))
/// ));
/// // So is the wrong number of arguments.
/// assert!(matches!(eval("md5", &[]), Err(ScalarError::Arity { .. })));
/// ```
pub enum ScalarError {
    /// No function with this name in the library. Field = the name.
    ///
    /// ```
    /// use kevy_scalar::{ScalarError, eval};
    /// let err = eval("frobnicate", &[]).unwrap_err();
    /// assert_eq!(err, ScalarError::UnknownFunction("frobnicate".into()));
    /// assert_eq!(err.to_string(), "unknown function: frobnicate");
    /// ```
    UnknownFunction(String),
    /// The function exists but not with this argument count.
    ///
    /// ```
    /// use kevy_scalar::{Scalar, ScalarError, eval};
    /// let two = [Scalar::Text("a".into()), Scalar::Text("b".into())];
    /// assert_eq!(eval("md5", &two), Err(ScalarError::Arity { func: "md5", got: 2 }));
    /// ```
    Arity {
        /// Function name.
        ///
        /// ```
        /// use kevy_scalar::{ScalarError, eval};
        /// let Err(ScalarError::Arity { func, .. }) = eval("MD5", &[]) else { panic!() };
        /// assert_eq!(func, "md5");
        /// ```
        func: &'static str,
        /// What the call supplied.
        ///
        /// ```
        /// use kevy_scalar::{ScalarError, eval};
        /// let Err(ScalarError::Arity { got, .. }) = eval("md5", &[]) else { panic!() };
        /// assert_eq!(got, 0);
        /// ```
        got: usize,
    },
    /// An argument had a type the function cannot take (PG would have
    /// refused the cast at parse time; the sql face reports this with
    /// the same vocabulary).
    ///
    /// ```
    /// use kevy_scalar::{Scalar, ScalarError, eval};
    /// let err = eval("md5", &[Scalar::Int(1)]).unwrap_err();
    /// assert_eq!(err, ScalarError::Type { func: "md5", arg: 0 });
    /// assert_eq!(err.to_string(), "md5: argument 1 has an unsupported type");
    /// ```
    Type {
        /// Function name.
        ///
        /// ```
        /// use kevy_scalar::{Scalar, ScalarError, eval};
        /// let Err(ScalarError::Type { func, .. }) = eval("md5", &[Scalar::Bool(true)]) else {
        ///     panic!()
        /// };
        /// assert_eq!(func, "md5");
        /// ```
        func: &'static str,
        /// 0-based argument position.
        ///
        /// ```
        /// use kevy_scalar::{Scalar, ScalarError, eval};
        /// let Err(ScalarError::Type { arg, .. }) = eval("md5", &[Scalar::Int(1)]) else {
        ///     panic!()
        /// };
        /// assert_eq!(arg, 0, "the first argument");
        /// ```
        arg: usize,
    },
    /// Arithmetic that PG raises on: division by zero, sqrt of a
    /// negative, integer overflow.
    ///
    /// ```
    /// use kevy_scalar::{Scalar, ScalarError, binop};
    /// let err = binop('/', &Scalar::Int(1), &Scalar::Int(0)).unwrap_err();
    /// assert_eq!(err, ScalarError::Domain { func: "/", what: "division by zero" });
    /// assert_eq!(err.to_string(), "/: division by zero");
    /// ```
    Domain {
        /// Function name.
        ///
        /// ```
        /// use kevy_scalar::{Scalar, ScalarError, binop};
        /// let Err(ScalarError::Domain { func, .. }) = binop('/', &Scalar::Int(1), &Scalar::Int(0))
        /// else {
        ///     panic!()
        /// };
        /// assert_eq!(func, "/");
        /// ```
        func: &'static str,
        /// PG's error phrase, e.g. `division by zero`.
        ///
        /// ```
        /// use kevy_scalar::{Scalar, ScalarError, binop};
        /// let Err(ScalarError::Domain { what, .. }) = binop('/', &Scalar::Int(1), &Scalar::Int(0))
        /// else {
        ///     panic!()
        /// };
        /// assert_eq!(what, "division by zero");
        /// ```
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
/// # Examples
///
/// ```
/// use kevy_scalar::{Scalar, eval};
/// // Names fold case, as PG folds unquoted identifiers.
/// assert_eq!(eval("UPPER", &[Scalar::Text("x".into())]).unwrap(), Scalar::Text("X".into()));
///
/// // Most functions are strict in NULL...
/// assert_eq!(eval("length", &[Scalar::Null]).unwrap(), Scalar::Null);
/// // ...and the null family looks through it, which is the point of it.
/// assert_eq!(
///     eval("coalesce", &[Scalar::Null, Scalar::Int(7)]).unwrap(),
///     Scalar::Int(7)
/// );
///
/// // The RFC 1321 vectors, lower-case hex as PG's md5() emits.
/// assert_eq!(
///     eval("md5", &[Scalar::Text(String::new())]).unwrap(),
///     Scalar::Text("d41d8cd98f00b204e9800998ecf8427e".into())
/// );
/// assert_eq!(
///     eval("md5", &[Scalar::Text("abc".into())]).unwrap(),
///     Scalar::Text("900150983cd24fb0d6963f7d28e17f72".into())
/// );
/// ```
pub fn eval(func: &str, args: &[Scalar]) -> Result<Scalar, ScalarError> {
    let name = func.to_ascii_lowercase();
    match name.as_str() {
        // ── strings ──
        "lower" | "upper" | "initcap" | "length" | "char_length" | "character_length"
        | "concat" | "concat_ws" | "trim" | "btrim" | "ltrim" | "rtrim" | "replace"
        | "split_part" | "repeat" | "lpad" | "rpad" | "strpos" | "position" | "left" | "right"
        | "reverse" | "translate" | "substr" | "substring" | "format" => strings::eval(&name, args),
        // ── math ──
        "floor" | "ceil" | "ceiling" | "round" | "trunc" | "mod" | "power" | "pow" | "sqrt"
        | "sign" | "abs" => math::eval(&name, args),
        // ── null family ──
        "coalesce" | "nullif" | "greatest" | "least" => nullfam::eval(&name, args),
        // ── date/time ──
        "extract" | "date_part" | "date_trunc" | "age" | "to_char" | "date_format"
        | "unix_timestamp" | "from_unixtime" => datetime::eval(&name, args),
        // ── regexp (vendored engine) ──
        "regexp_replace" | "regexp_matches" | "regexp_split_to_array" => regexp::eval(&name, args),
        // ── hash ──
        "md5" => match args {
            [Scalar::Null] => Ok(Scalar::Null),
            [Scalar::Text(t)] => Ok(Scalar::Text(md5::md5_hex(t.as_bytes()))),
            [_] => Err(ScalarError::Type { func: "md5", arg: 0 }),
            _ => Err(ScalarError::Arity { func: "md5", got: args.len() }),
        },
        _ => Err(ScalarError::UnknownFunction(func.to_string())),
    }
}
