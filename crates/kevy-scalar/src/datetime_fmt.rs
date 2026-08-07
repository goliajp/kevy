//! Parsing and text rendering for the date/time variants, plus the
//! `to_char` template subset. Forms are PG's text output exactly —
//! funcgate compares these strings byte for byte against the probe
//! expectations ("1 year 2 mons", "02:00:00", trailing `.000000`
//! suppressed).

use crate::datetime::{MICROS_PER_DAY, MICROS_PER_SEC};
use crate::{Scalar, ScalarError};

/// Parse `YYYY-MM-DD[ HH:MM:SS[.ffffff]]` (a `T` separator is accepted
/// too) into epoch microseconds. `None` on anything malformed — the
/// sql face refuses by name, this module never guesses.
#[must_use]
pub fn parse_timestamp(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, time) = match s.split_once([' ', 'T']) {
        None => (s, None),
        Some((d, t)) => (d, Some(t)),
    };
    let days = parse_date(date)?;
    let mut us = days * MICROS_PER_DAY;
    if let Some(t) = time {
        let (hms, frac) = match t.split_once('.') {
            None => (t, 0i64),
            Some((h, f)) => {
                if f.is_empty() || f.len() > 6 || f.bytes().any(|b| !b.is_ascii_digit()) {
                    return None;
                }
                // Right-pad to microseconds: ".5" is 500000.
                let scale = 10i64.pow(6 - f.len() as u32);
                (h, f.parse::<i64>().ok()? * scale)
            }
        };
        let mut it = hms.split(':');
        let h: i64 = it.next()?.parse().ok()?;
        let m: i64 = it.next()?.parse().ok()?;
        let sec: i64 = it.next().unwrap_or("0").parse().ok()?;
        if it.next().is_some() || h > 23 || m > 59 || sec > 59 {
            return None;
        }
        us += (h * 3600 + m * 60 + sec) * MICROS_PER_SEC + frac;
    }
    Some(us)
}

/// Parse `YYYY-MM-DD` into days since the epoch.
#[must_use]
pub fn parse_date(s: &str) -> Option<i64> {
    let mut it = s.trim().split('-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: u32 = it.next()?.parse().ok()?;
    let d: u32 = it.next()?.parse().ok()?;
    if it.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let c = kevy_time::Civil { y, m, d, h: 0, min: 0, s: 0 };
    let secs = kevy_time::epoch_from_civil(c);
    // Round-trip check rejects Feb 30 and friends.
    let back = kevy_time::civil_from_epoch(secs);
    if (back.y, back.m, back.d) != (y, m, d) {
        return None;
    }
    Some(secs / 86_400)
}

/// Parse a PG interval literal: whitespace-separated `<n> <unit>`
/// pairs (`1 year 2 months`, `-3 days`, `90 minutes`) into the
/// three-component `(months, days, micros)`.
#[must_use]
pub fn parse_interval(s: &str) -> Option<(i64, i64, i64)> {
    let (mut months, mut days, mut micros) = (0i64, 0i64, 0i64);
    let mut it = s.split_whitespace();
    let mut any = false;
    while let Some(tok) = it.next() {
        let n: i64 = tok.parse().ok()?;
        let unit = it.next()?.trim_end_matches('s');
        match unit {
            "year" | "yr" => months += n * 12,
            "month" | "mon" => months += n,
            "week" => days += n * 7,
            "day" => days += n,
            "hour" => micros += n * 3600 * MICROS_PER_SEC,
            "minute" | "min" => micros += n * 60 * MICROS_PER_SEC,
            "second" | "sec" => micros += n * MICROS_PER_SEC,
            _ => return None,
        }
        any = true;
    }
    if any { Some((months, days, micros)) } else { None }
}

/// `YYYY-MM-DD HH:MM:SS[.ffffff]` — fraction only when non-zero.
#[must_use]
pub fn render_timestamp(us: i64) -> String {
    let secs = us.div_euclid(MICROS_PER_SEC);
    let frac = us.rem_euclid(MICROS_PER_SEC);
    let c = kevy_time::civil_from_epoch(secs);
    let mut out = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        c.y, c.m, c.d, c.h, c.min, c.s
    );
    if frac != 0 {
        out.push_str(format!(".{frac:06}").trim_end_matches('0'));
    }
    out
}

/// `YYYY-MM-DD`.
#[must_use]
pub fn render_date(days: i64) -> String {
    let c = kevy_time::civil_from_epoch(days * 86_400);
    format!("{:04}-{:02}-{:02}", c.y, c.m, c.d)
}

/// PG's interval output: `N year(s) N mon(s) N day(s) HH:MM:SS`, each
/// piece only when non-zero, `00:00:00` alone when everything is. The
/// components render independently — `1 day -12:00:00` stays exactly
/// that (probe 10).
#[must_use]
pub fn render_interval(months: i64, days: i64, micros: i64) -> String {
    let mut parts: Vec<String> = Vec::new();
    let (y, mon) = (months / 12, months % 12);
    let rem = micros;
    if y != 0 {
        parts.push(format!("{y} year{}", if y.abs() == 1 { "" } else { "s" }));
    }
    if mon != 0 {
        parts.push(format!("{mon} mon{}", if mon.abs() == 1 { "" } else { "s" }));
    }
    if days != 0 {
        parts.push(format!("{days} day{}", if days.abs() == 1 { "" } else { "s" }));
    }
    if rem != 0 || parts.is_empty() {
        let neg = rem < 0;
        let r = rem.abs();
        let (h, m) = (r / (3600 * MICROS_PER_SEC), r / (60 * MICROS_PER_SEC) % 60);
        let s = r / MICROS_PER_SEC % 60;
        let frac = r % MICROS_PER_SEC;
        let mut t = format!("{}{h:02}:{m:02}:{s:02}", if neg { "-" } else { "" });
        if frac != 0 {
            t.push_str(format!(".{frac:06}").trim_end_matches('0'));
        }
        parts.push(t);
    }
    parts.join(" ")
}

/// The `to_char(timestamp, template)` subset the corpus exercises:
/// `YYYY MM DD HH24 MI SS` plus literal separators. Any other pattern
/// letter is refused by name — a silently wrong render is worse than
/// a named gap.
pub(crate) fn to_char(args: &[Scalar]) -> Result<Scalar, ScalarError> {
    const FUNC: &str = "to_char";
    let (us, tpl) = match args {
        [Scalar::Null, _] | [_, Scalar::Null] => return Ok(Scalar::Null),
        [Scalar::Timestamp(us), Scalar::Text(t)] => (*us, t.as_str()),
        [Scalar::Date(d), Scalar::Text(t)] => (d * MICROS_PER_DAY, t.as_str()),
        [_, _] => return Err(ScalarError::Type { func: FUNC, arg: 0 }),
        _ => return Err(ScalarError::Arity { func: FUNC, got: args.len() }),
    };
    let c = kevy_time::civil_from_epoch(us.div_euclid(MICROS_PER_SEC));
    let mut out = String::with_capacity(tpl.len());
    let mut rest = tpl;
    while !rest.is_empty() {
        let (token, len): (String, usize) = if rest.starts_with("YYYY") {
            (format!("{:04}", c.y), 4)
        } else if rest.starts_with("HH24") {
            (format!("{:02}", c.h), 4)
        } else if rest.starts_with("MM") {
            (format!("{:02}", c.m), 2)
        } else if rest.starts_with("DD") {
            (format!("{:02}", c.d), 2)
        } else if rest.starts_with("MI") {
            (format!("{:02}", c.min), 2)
        } else if rest.starts_with("SS") {
            (format!("{:02}", c.s), 2)
        } else {
            let ch = rest.chars().next().expect("non-empty rest");
            if ch.is_ascii_alphanumeric() {
                // An unrecognized pattern letter — refuse, never guess.
                return Err(ScalarError::Domain {
                    func: FUNC,
                    what: "unsupported to_char template pattern",
                });
            }
            (ch.to_string(), ch.len_utf8())
        };
        out.push_str(&token);
        rest = &rest[len..];
    }
    Ok(Scalar::Text(out))
}
