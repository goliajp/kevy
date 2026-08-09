//! Date/time functions over the typed variants. The field/type matrix
//! is PG 18's, which is stricter than it looks (probe 07/11):
//! `extract` REFUSES time-of-day fields on a plain DATE while
//! `date_part` promotes the date to midnight and answers 0 — the two
//! spellings are not aliases. Calendar math rides kevy-time; this
//! module never touches a clock (`now()` is rewritten to a literal at
//! the sql face, the probe-08 precedent).

use crate::datetime_fmt as fmt;
use crate::{Scalar, ScalarError};

pub(crate) const MICROS_PER_SEC: i64 = 1_000_000;
pub(crate) const MICROS_PER_DAY: i64 = 86_400 * MICROS_PER_SEC;

pub(crate) fn eval(name: &str, args: &[Scalar]) -> Result<Scalar, ScalarError> {
    match name {
        "extract" => field_of("extract", args, false),
        "date_part" => field_of("date_part", args, true),
        "date_trunc" => date_trunc(args),
        "age" => age(args),
        "to_char" => fmt::to_char(args),
        "date_format" => fmt::date_format(args),
        "unix_timestamp" => fmt::unix_timestamp(args),
        "from_unixtime" => fmt::from_unixtime(args),
        _ => Err(ScalarError::UnknownFunction(name.to_string())),
    }
}

/// `extract(field FROM x)` / `date_part(field, x)` — the sql face
/// normalizes both spellings to `(field, value)` argument order.
fn field_of(func: &'static str, args: &[Scalar], promote: bool) -> Result<Scalar, ScalarError> {
    let (field, v) = match args {
        [Scalar::Null, _] | [_, Scalar::Null] => return Ok(Scalar::Null),
        [Scalar::Text(f), v] => (f.to_ascii_lowercase(), v),
        [_, _] => return Err(ScalarError::Type { func, arg: 0 }),
        _ => return Err(ScalarError::Arity { func, got: args.len() }),
    };
    match v {
        Scalar::Timestamp(us) => field_of_ts(func, &field, *us),
        Scalar::Date(days) => {
            if is_time_field(&field) && !promote {
                // PG 18.4: `unit "hour" not supported for type date`.
                return Err(ScalarError::Domain {
                    func,
                    what: "this unit is not supported for type date",
                });
            }
            field_of_ts(func, &field, days * MICROS_PER_DAY)
        }
        Scalar::Interval { months, days, micros } => {
            field_of_interval(func, &field, *months, *days, *micros)
        }
        _ => Err(ScalarError::Type { func, arg: 1 }),
    }
}

fn is_time_field(field: &str) -> bool {
    matches!(field, "hour" | "minute" | "second" | "milliseconds" | "microseconds")
}

fn field_of_ts(func: &'static str, field: &str, us: i64) -> Result<Scalar, ScalarError> {
    let secs = us.div_euclid(MICROS_PER_SEC);
    let frac_us = us.rem_euclid(MICROS_PER_SEC);
    let c = kevy_time::civil_from_epoch(secs);
    let out = match field {
        "year" => c.y as f64,
        "quarter" => f64::from((c.m - 1) / 3 + 1),
        "month" => f64::from(c.m),
        "day" => f64::from(c.d),
        "hour" => f64::from(c.h),
        "minute" => f64::from(c.min),
        "second" => f64::from(c.s) + frac_us as f64 / 1e6,
        "epoch" => us as f64 / 1e6,
        "dow" => (us.div_euclid(MICROS_PER_DAY) + 4).rem_euclid(7) as f64,
        "doy" => doy(c) as f64,
        _ => {
            return Err(ScalarError::Domain { func, what: "unknown extract field" });
        }
    };
    Ok(Scalar::Float(out))
}

/// Day of year: distance from Jan 1 of the same year, 1-based.
fn doy(c: kevy_time::Civil) -> i64 {
    let jan1 = kevy_time::epoch_from_civil(kevy_time::Civil { m: 1, d: 1, h: 0, min: 0, s: 0, ..c });
    let this = kevy_time::epoch_from_civil(kevy_time::Civil { h: 0, min: 0, s: 0, ..c });
    (this - jan1) / 86_400 + 1
}

/// Interval component decomposition (probe 11): each field reads its
/// own component — days never fold into hours or vice versa.
fn field_of_interval(
    func: &'static str,
    field: &str,
    months: i64,
    days: i64,
    micros: i64,
) -> Result<Scalar, ScalarError> {
    let out = match field {
        "year" => (months / 12) as f64,
        "month" => (months % 12) as f64,
        "day" => days as f64,
        "hour" => (micros / (3600 * MICROS_PER_SEC)) as f64,
        "minute" => (micros % (3600 * MICROS_PER_SEC) / (60 * MICROS_PER_SEC)) as f64,
        "second" => (micros % (60 * MICROS_PER_SEC)) as f64 / 1e6,
        // PG's epoch convention: a month counts 30 days.
        "epoch" => (months * 30 + days) as f64 * 86_400.0 + micros as f64 / 1e6,
        _ => {
            return Err(ScalarError::Domain { func, what: "unknown extract field" });
        }
    };
    Ok(Scalar::Float(out))
}

/// Round a timestamp down to the requested boundary.
fn date_trunc(args: &[Scalar]) -> Result<Scalar, ScalarError> {
    const FUNC: &str = "date_trunc";
    let (field, us) = match args {
        [Scalar::Null, _] | [_, Scalar::Null] => return Ok(Scalar::Null),
        [Scalar::Text(f), Scalar::Timestamp(us)] => (f.to_ascii_lowercase(), *us),
        [Scalar::Text(f), Scalar::Date(d)] => (f.to_ascii_lowercase(), d * MICROS_PER_DAY),
        [_, _] => return Err(ScalarError::Type { func: FUNC, arg: 0 }),
        _ => return Err(ScalarError::Arity { func: FUNC, got: args.len() }),
    };
    let secs = us.div_euclid(MICROS_PER_SEC);
    let mut c = kevy_time::civil_from_epoch(secs);
    match field.as_str() {
        "year" => (c.m, c.d, c.h, c.min, c.s) = (1, 1, 0, 0, 0),
        "quarter" => (c.m, c.d, c.h, c.min, c.s) = ((c.m - 1) / 3 * 3 + 1, 1, 0, 0, 0),
        "month" => (c.d, c.h, c.min, c.s) = (1, 0, 0, 0),
        "week" => {
            // ISO week starts Monday; dow: 0=Sun..6=Sat.
            let dow = (secs.div_euclid(86_400) + 4).rem_euclid(7);
            let back = (dow + 6) % 7;
            let day0 = kevy_time::epoch_from_civil(kevy_time::Civil { h: 0, min: 0, s: 0, ..c });
            return Ok(Scalar::Timestamp((day0 - back * 86_400) * MICROS_PER_SEC));
        }
        "day" => (c.h, c.min, c.s) = (0, 0, 0),
        "hour" => (c.min, c.s) = (0, 0),
        "minute" => c.s = 0,
        "second" => {
            return Ok(Scalar::Timestamp(secs * MICROS_PER_SEC));
        }
        _ => {
            return Err(ScalarError::Domain { func: FUNC, what: "unknown truncation field" });
        }
    }
    Ok(Scalar::Timestamp(kevy_time::epoch_from_civil(c) * MICROS_PER_SEC))
}

/// `age(later, earlier)` — PG's calendar decomposition: whole years
/// and months first (borrowing so every component keeps the overall
/// sign), then exact days and time from what remains.
fn age(args: &[Scalar]) -> Result<Scalar, ScalarError> {
    const FUNC: &str = "age";
    let (a, b) = match args {
        [Scalar::Null, _] | [_, Scalar::Null] => return Ok(Scalar::Null),
        [Scalar::Timestamp(a), Scalar::Timestamp(b)] => (*a, *b),
        [Scalar::Date(a), Scalar::Date(b)] => (a * MICROS_PER_DAY, b * MICROS_PER_DAY),
        [_, _] => return Err(ScalarError::Type { func: FUNC, arg: 0 }),
        _ => return Err(ScalarError::Arity { func: FUNC, got: args.len() }),
    };
    let (later, earlier, neg) = if a >= b { (a, b, false) } else { (b, a, true) };
    let (ls, es) = (later.div_euclid(MICROS_PER_SEC), earlier.div_euclid(MICROS_PER_SEC));
    let (lc, ec) = (kevy_time::civil_from_epoch(ls), kevy_time::civil_from_epoch(es));
    let mut months = (lc.y - ec.y) * 12 + i64::from(lc.m) - i64::from(ec.m);
    // Borrow a month whenever the shifted-earlier lands past later.
    while kevy_time::add_months(es, months) * MICROS_PER_SEC
        + earlier.rem_euclid(MICROS_PER_SEC)
        > later
    {
        months -= 1;
    }
    let anchored = kevy_time::add_months(es, months) * MICROS_PER_SEC
        + earlier.rem_euclid(MICROS_PER_SEC);
    let rest = later - anchored;
    let (days, micros) = (rest / MICROS_PER_DAY, rest % MICROS_PER_DAY);
    Ok(if neg {
        Scalar::Interval { months: -months, days: -days, micros: -micros }
    } else {
        Scalar::Interval { months, days, micros }
    })
}
