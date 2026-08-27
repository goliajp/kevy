//! Pure calendar arithmetic on epoch seconds — the date-arithmetic
//! stone (R4a's last engine-side gap). Time columns stay declared
//! i64 (epoch seconds; units belong to the caller, the WINDOW
//! philosophy) — this crate is the query-side arithmetic that turns
//! a human bound into that i64, never a new column type.
//!
//! Everything here is a pure function: `now` is always an argument,
//! the stone never touches a clock. Proleptic Gregorian, UTC only
//! (zone conversion is the application's — refused by name at the
//! surface, not silently guessed here).
//!
//! The civil conversion is the standard integer-arithmetic algorithm
//! (era/year-of-era/day-of-year decomposition over 400-year cycles),
//! exact over the whole i64 day range the epoch can reach.

#![forbid(unsafe_code)]

const SECS_PER_DAY: i64 = 86_400;

/// One civil timestamp: year, month (1-12), day (1-31), hour,
/// minute, second — UTC, proleptic Gregorian.
/// # Examples
///
/// ```
/// let c = kevy_time::civil_from_epoch(0);
/// assert_eq!((c.y, c.m, c.d), (1970, 1, 1));
/// assert_eq!((c.h, c.min, c.s), (0, 0, 0));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Civil {
    /// Year (proleptic Gregorian; negative epochs decode correctly).
    pub y: i64,
    /// Month, 1-12.
    pub m: u32,
    /// Day of month, 1-31.
    pub d: u32,
    /// Hour, 0-23.
    pub h: u32,
    /// Minute, 0-59.
    pub min: u32,
    /// Second, 0-59.
    pub s: u32,
}

/// Days since the epoch for a civil date.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = i64::from(if m > 2 { m - 3 } else { m + 9 });
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Civil date for days since the epoch.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Decode epoch seconds to a civil timestamp.
/// # Examples
///
/// ```
/// let c = kevy_time::civil_from_epoch(0);
/// assert_eq!((c.y, c.m, c.d, c.h, c.min, c.s), (1970, 1, 1, 0, 0, 0));
/// ```
///
/// Negative seconds run backwards through the epoch rather than clamping
/// at it.
///
/// ```
/// let c = kevy_time::civil_from_epoch(-1);
/// assert_eq!((c.y, c.m, c.d, c.h, c.min, c.s), (1969, 12, 31, 23, 59, 59));
/// ```
pub fn civil_from_epoch(secs: i64) -> Civil {
    let days = secs.div_euclid(SECS_PER_DAY);
    let rem = secs.rem_euclid(SECS_PER_DAY) as u32;
    let (y, m, d) = civil_from_days(days);
    Civil { y, m, d, h: rem / 3600, min: rem / 60 % 60, s: rem % 60 }
}

/// Encode a civil timestamp to epoch seconds. The caller supplies
/// in-range fields; out-of-range months/days would decode to a
/// different date, which is why the parser validates before calling.
///
/// # Examples
///
/// It is the exact inverse of [`civil_from_epoch`] for every in-range
/// timestamp, which is the property the parser depends on:
///
/// ```
/// use kevy_time::{civil_from_epoch, epoch_from_civil};
/// for t in [0i64, 1, -1, 951_782_400, 1_700_000_000, -2_208_988_800] {
///     assert_eq!(epoch_from_civil(civil_from_epoch(t)), t, "round trip at {t}");
/// }
/// ```
pub fn epoch_from_civil(c: Civil) -> i64 {
    days_from_civil(c.y, c.m, c.d) * SECS_PER_DAY
        + i64::from(c.h) * 3600
        + i64::from(c.min) * 60
        + i64::from(c.s)
}

/// The last day of a month (leap-aware).
fn last_day(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
                29
            } else {
                28
            }
        }
    }
}

/// Add `n` calendar months (negative subtracts), clamping the day to
/// the target month's end — Jan 31 + 1mo = Feb 28 (29 in a leap
/// year), the convention every calendar API converges on.
///
/// # Examples
///
/// The clamp is the whole point — January 31 has no counterpart in
/// February, and every calendar API converges on the month's end rather
/// than spilling into March:
///
/// ```
/// use kevy_time::{add_months, civil_from_epoch, epoch_from_civil, Civil};
/// let jan31 = epoch_from_civil(Civil { y: 2023, m: 1, d: 31, h: 0, min: 0, s: 0 });
/// let feb = civil_from_epoch(add_months(jan31, 1));
/// assert_eq!((feb.y, feb.m, feb.d), (2023, 2, 28));
///
/// let leap = epoch_from_civil(Civil { y: 2024, m: 1, d: 31, h: 0, min: 0, s: 0 });
/// let feb = civil_from_epoch(add_months(leap, 1));
/// assert_eq!((feb.y, feb.m, feb.d), (2024, 2, 29));
/// ```
///
/// Because of that clamp, adding a month is **not** reversible by
/// subtracting one:
///
/// ```
/// use kevy_time::{add_months, civil_from_epoch, epoch_from_civil, Civil};
/// let jan31 = epoch_from_civil(Civil { y: 2023, m: 1, d: 31, h: 0, min: 0, s: 0 });
/// let back = civil_from_epoch(add_months(add_months(jan31, 1), -1));
/// assert_eq!((back.m, back.d), (1, 28), "the day did not come back");
/// ```
///
/// Negative `n` walks backwards across a year boundary:
///
/// ```
/// use kevy_time::{add_months, civil_from_epoch, epoch_from_civil, Civil};
/// let mar = epoch_from_civil(Civil { y: 2024, m: 3, d: 15, h: 0, min: 0, s: 0 });
/// let c = civil_from_epoch(add_months(mar, -4));
/// assert_eq!((c.y, c.m, c.d), (2023, 11, 15));
/// ```
pub fn add_months(secs: i64, n: i64) -> i64 {
    let c = civil_from_epoch(secs);
    let months = c.y * 12 + i64::from(c.m) - 1 + n;
    let (y, m) = (months.div_euclid(12), (months.rem_euclid(12) + 1) as u32);
    let d = c.d.min(last_day(y, m));
    epoch_from_civil(Civil { y, m, d, ..c })
}

/// Evaluate one `@` query-bound expression against the caller's
/// `now`. `None` on anything malformed — the surface refuses by
/// name, this stone never guesses.
///
/// Grammar: `@now`, `@now±<n><unit>` with unit s|m|h|d|w (plain
/// second arithmetic) or mo|y (calendar months via [`add_months`]),
/// `@YYYY-MM-DD` (midnight) and `@YYYY-MM-DDThh:mm:ss`.
///
/// # Examples
///
/// ```
/// use kevy_time::eval;
/// let now = 1_700_000_000;
/// assert_eq!(eval(b"@now", now), Some(now));
/// assert_eq!(eval(b"@now-1h", now), Some(now - 3600));
/// assert_eq!(eval(b"@now+7d", now), Some(now + 7 * 86_400));
/// assert_eq!(eval(b"@1970-01-01", now), Some(0));
/// assert_eq!(eval(b"@1970-01-01T00:00:01", now), Some(1));
/// ```
///
/// `mo` and `y` go through [`add_months`], so they carry its clamp rather
/// than a fixed number of seconds:
///
/// ```
/// use kevy_time::{eval, epoch_from_civil, civil_from_epoch, Civil};
/// let jan31 = epoch_from_civil(Civil { y: 2023, m: 1, d: 31, h: 0, min: 0, s: 0 });
/// let c = civil_from_epoch(eval(b"@now+1mo", jan31).unwrap());
/// assert_eq!((c.m, c.d), (2, 28));
/// ```
///
/// Anything malformed is `None`. The stone never guesses — a caller that
/// wants a default has to say so itself:
///
/// ```
/// use kevy_time::eval;
/// for bad in [&b"now"[..], b"@", b"@now+", b"@now+5", b"@now+5x", b"@1970-1-1", b"@tomorrow"] {
///     assert_eq!(eval(bad, 0), None, "{:?} should not parse", core::str::from_utf8(bad));
/// }
/// ```
pub fn eval(expr: &[u8], now: i64) -> Option<i64> {
    let body = expr.strip_prefix(b"@")?;
    if let Some(rest) = body.strip_prefix(b"now") {
        if rest.is_empty() {
            return Some(now);
        }
        let (sign, rest) = match rest.first()? {
            b'+' => (1i64, &rest[1..]),
            b'-' => (-1i64, &rest[1..]),
            _ => return None,
        };
        let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
        if digits == 0 {
            return None;
        }
        let n: i64 = std::str::from_utf8(&rest[..digits]).ok()?.parse().ok()?;
        return match &rest[digits..] {
            b"s" => now.checked_add(sign * n),
            b"m" => now.checked_add(sign.checked_mul(n.checked_mul(60)?)?),
            b"h" => now.checked_add(sign.checked_mul(n.checked_mul(3600)?)?),
            b"d" => now.checked_add(sign.checked_mul(n.checked_mul(SECS_PER_DAY)?)?),
            b"w" => now.checked_add(sign.checked_mul(n.checked_mul(7 * SECS_PER_DAY)?)?),
            b"mo" => Some(add_months(now, sign * n)),
            b"y" => Some(add_months(now, sign.checked_mul(n.checked_mul(12)?)?)),
            _ => None,
        };
    }
    parse_literal(body)
}

/// `YYYY-MM-DD` or `YYYY-MM-DDThh:mm:ss`, validated against the real
/// calendar (a Feb 30 is a refusal, not a wraparound).
fn parse_literal(b: &[u8]) -> Option<i64> {
    let num = |s: &[u8]| -> Option<i64> {
        (!s.is_empty() && s.iter().all(u8::is_ascii_digit))
            .then(|| std::str::from_utf8(s).ok()?.parse().ok())
            .flatten()
    };
    let (date, time) = match b.len() {
        10 => (b, None),
        19 => {
            if b[10] != b'T' {
                return None;
            }
            (&b[..10], Some(&b[11..]))
        }
        _ => return None,
    };
    if date[4] != b'-' || date[7] != b'-' {
        return None;
    }
    let (y, m, d) = (num(&date[..4])?, num(&date[5..7])? as u32, num(&date[8..10])? as u32);
    if !(1..=12).contains(&m) || d < 1 || d > last_day(y, m) {
        return None;
    }
    let (h, min, s) = match time {
        None => (0, 0, 0),
        Some(t) => {
            if t[2] != b':' || t[5] != b':' {
                return None;
            }
            let (h, min, s) = (num(&t[..2])? as u32, num(&t[3..5])? as u32, num(&t[6..8])? as u32);
            if h > 23 || min > 59 || s > 59 {
                return None;
            }
            (h, min, s)
        }
    };
    Some(epoch_from_civil(Civil { y, m, d, h, min, s }))
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
