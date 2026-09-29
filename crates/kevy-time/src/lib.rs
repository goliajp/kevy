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
//!
//! ```
//! use kevy_time::{Civil, add_months};
//!
//! // Epoch seconds in, calendar out, and back again.
//! let c = Civil::from_epoch(1_700_000_000);
//! assert_eq!((c.year(), c.month(), c.day()), (2023, 11, 14));
//! assert_eq!((c.hour(), c.minute(), c.second()), (22, 13, 20));
//! assert_eq!(c.to_epoch(), 1_700_000_000);
//!
//! // Month arithmetic clamps rather than spilling into the next month:
//! // 31 January plus one month is the last day of February, and 2024 is
//! // a leap year.
//! let jan31 = Civil::from_date(2024, 1, 31).ok_or("not a date")?.to_epoch();
//! let feb = Civil::from_epoch(add_months(jan31, 1));
//! assert_eq!((feb.month(), feb.day()), (2, 29));
//! # Ok::<(), &str>(())
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

const SECS_PER_DAY: i64 = 86_400;

/// One civil timestamp: year, month (1-12), day (1-31), hour,
/// minute, second — UTC, proleptic Gregorian.
///
/// Always a real calendar instant that an `i64` epoch can hold: it is
/// built from epoch seconds ([`Civil::from_epoch`]) or from fields that
/// are checked ([`Civil::from_date`], [`Civil::with_time`]), so
/// [`Civil::to_epoch`] cannot fail. Ordering is chronological.
///
/// # Examples
///
/// ```
/// let c = kevy_time::Civil::from_epoch(0);
/// assert_eq!((c.year(), c.month(), c.day()), (1970, 1, 1));
/// assert_eq!((c.hour(), c.minute(), c.second()), (0, 0, 0));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Civil {
    y: i64,
    m: u32,
    d: u32,
    h: u32,
    min: u32,
    s: u32,
}

/// Years past this are refused before any day arithmetic: 2^63 seconds
/// is about 292 billion years, and the day count itself overflows for
/// years far beyond that.
const MAX_YEAR: i64 = 300_000_000_000;

impl Civil {
    /// Decode epoch seconds.
    ///
    /// ```
    /// let c = kevy_time::Civil::from_epoch(0);
    /// assert_eq!((c.year(), c.month(), c.day()), (1970, 1, 1));
    /// assert_eq!((c.hour(), c.minute(), c.second()), (0, 0, 0));
    /// ```
    ///
    /// Negative seconds run backwards through the epoch rather than
    /// clamping at it.
    ///
    /// ```
    /// let c = kevy_time::Civil::from_epoch(-1);
    /// assert_eq!((c.year(), c.month(), c.day()), (1969, 12, 31));
    /// assert_eq!((c.hour(), c.minute(), c.second()), (23, 59, 59));
    /// ```
    #[must_use]
    pub fn from_epoch(secs: i64) -> Self {
        let days = secs.div_euclid(SECS_PER_DAY);
        let rem = secs.rem_euclid(SECS_PER_DAY) as u32;
        let (y, m, d) = civil_from_days(days);
        Self { y, m, d, h: rem / 3600, min: rem / 60 % 60, s: rem % 60 }
    }

    /// Midnight at the start of a date, or `None` when the date is not
    /// on the calendar (month 13, 30 February) or lies outside what an
    /// `i64` epoch can hold.
    ///
    /// ```
    /// use kevy_time::Civil;
    /// assert_eq!(Civil::from_date(1970, 1, 1).map(Civil::to_epoch), Some(0));
    /// assert_eq!(Civil::from_date(2024, 2, 29).map(Civil::day), Some(29));
    /// assert_eq!(Civil::from_date(2023, 2, 29), None);
    /// assert_eq!(Civil::from_date(2023, 13, 1), None);
    /// assert_eq!(Civil::from_date(i64::MAX / 2, 1, 1), None);
    /// ```
    #[must_use]
    pub fn from_date(y: i64, m: u32, d: u32) -> Option<Self> {
        if !(1..=12).contains(&m) || d == 0 || d > last_day(y, m) {
            return None;
        }
        Self { y, m, d, h: 0, min: 0, s: 0 }.checked()
    }

    /// The same date at another time of day, or `None` when the time is
    /// not on the clock (hour 24, minute 60) or the instant lies outside
    /// what an `i64` epoch can hold.
    ///
    /// ```
    /// use kevy_time::Civil;
    /// let c = Civil::from_date(1970, 1, 1).and_then(|c| c.with_time(0, 0, 1));
    /// assert_eq!(c.map(Civil::to_epoch), Some(1));
    /// assert_eq!(Civil::from_epoch(0).with_time(24, 0, 0), None);
    /// ```
    #[must_use]
    pub fn with_time(self, h: u32, min: u32, s: u32) -> Option<Self> {
        if h > 23 || min > 59 || s > 59 {
            return None;
        }
        Self { h, min, s, ..self }.checked()
    }

    /// Encode to epoch seconds — the exact inverse of
    /// [`Civil::from_epoch`].
    ///
    /// ```
    /// use kevy_time::Civil;
    /// for t in [0i64, 1, -1, 951_782_400, 1_700_000_000, -2_208_988_800, i64::MAX, i64::MIN] {
    ///     assert_eq!(Civil::from_epoch(t).to_epoch(), t, "round trip at {t}");
    /// }
    /// ```
    #[must_use]
    pub fn to_epoch(self) -> i64 {
        // exact: every constructor checked that the instant fits
        self.wide_epoch() as i64
    }

    /// Year (proleptic Gregorian; negative epochs decode correctly).
    ///
    /// ```
    /// assert_eq!(kevy_time::Civil::from_epoch(-1).year(), 1969);
    /// ```
    #[must_use]
    pub fn year(self) -> i64 {
        self.y
    }

    /// Month, 1-12.
    ///
    /// ```
    /// assert_eq!(kevy_time::Civil::from_epoch(0).month(), 1);
    /// ```
    #[must_use]
    pub fn month(self) -> u32 {
        self.m
    }

    /// Day of month, 1-31.
    ///
    /// ```
    /// assert_eq!(kevy_time::Civil::from_epoch(-1).day(), 31);
    /// ```
    #[must_use]
    pub fn day(self) -> u32 {
        self.d
    }

    /// Hour, 0-23.
    ///
    /// ```
    /// assert_eq!(kevy_time::Civil::from_epoch(3600).hour(), 1);
    /// ```
    #[must_use]
    pub fn hour(self) -> u32 {
        self.h
    }

    /// Minute, 0-59.
    ///
    /// ```
    /// assert_eq!(kevy_time::Civil::from_epoch(60).minute(), 1);
    /// ```
    #[must_use]
    pub fn minute(self) -> u32 {
        self.min
    }

    /// Second, 0-59.
    ///
    /// ```
    /// assert_eq!(kevy_time::Civil::from_epoch(59).second(), 59);
    /// ```
    #[must_use]
    pub fn second(self) -> u32 {
        self.s
    }

    /// `self` when its instant fits an `i64` epoch. The fields are
    /// already on the calendar; this is the range half of the invariant.
    fn checked(self) -> Option<Self> {
        if !(-MAX_YEAR..=MAX_YEAR).contains(&self.y) {
            return None;
        }
        i64::try_from(self.wide_epoch()).ok()?;
        Some(self)
    }

    /// Epoch seconds in `i128`: on the first day an `i64` epoch reaches,
    /// midnight itself is before `i64::MIN`, so the sum has to be formed
    /// wider than its result.
    fn wide_epoch(self) -> i128 {
        i128::from(days_from_civil(self.y, self.m, self.d)) * i128::from(SECS_PER_DAY)
            + i128::from(self.h * 3600 + self.min * 60 + self.s)
    }
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
/// use kevy_time::{add_months, Civil};
/// let jan31 = Civil::from_date(2023, 1, 31).ok_or("not a date")?.to_epoch();
/// let feb = Civil::from_epoch(add_months(jan31, 1));
/// assert_eq!((feb.year(), feb.month(), feb.day()), (2023, 2, 28));
///
/// let leap = Civil::from_date(2024, 1, 31).ok_or("not a date")?.to_epoch();
/// let feb = Civil::from_epoch(add_months(leap, 1));
/// assert_eq!((feb.year(), feb.month(), feb.day()), (2024, 2, 29));
/// # Ok::<(), &str>(())
/// ```
///
/// Because of that clamp, adding a month is **not** reversible by
/// subtracting one:
///
/// ```
/// use kevy_time::{add_months, Civil};
/// let jan31 = Civil::from_date(2023, 1, 31).ok_or("not a date")?.to_epoch();
/// let back = Civil::from_epoch(add_months(add_months(jan31, 1), -1));
/// assert_eq!((back.month(), back.day()), (1, 28), "the day did not come back");
/// # Ok::<(), &str>(())
/// ```
///
/// Negative `n` walks backwards across a year boundary:
///
/// ```
/// use kevy_time::{add_months, Civil};
/// let mar = Civil::from_date(2024, 3, 15).ok_or("not a date")?.to_epoch();
/// let c = Civil::from_epoch(add_months(mar, -4));
/// assert_eq!((c.year(), c.month(), c.day()), (2023, 11, 15));
/// # Ok::<(), &str>(())
/// ```
pub fn add_months(secs: i64, n: i64) -> i64 {
    checked_add_months(secs, n).unwrap_or(if n < 0 { i64::MIN } else { i64::MAX })
}

/// [`add_months`], answering `None` instead of saturating when the result
/// leaves the range an `i64` epoch can hold.
///
/// The arithmetic here is unbounded in `n`: the month count, the era
/// split and the final multiply by seconds-per-day all overflow long
/// before `n` does. Unchecked, `@now+9223372036854775807mo` panicked in
/// debug and returned a 1969 timestamp in release, while the same
/// expression written in days was correctly refused — five of the seven
/// units in [`eval`] rejected overflow and two did not.
///
/// # Examples
///
/// ```
/// use kevy_time::checked_add_months;
/// assert_eq!(checked_add_months(0, 1), Some(2_678_400));  // 1970-02-01
/// assert_eq!(checked_add_months(0, i64::MAX), None);
/// assert_eq!(checked_add_months(0, i64::MIN), None);
/// ```
#[must_use]
pub fn checked_add_months(secs: i64, n: i64) -> Option<i64> {
    let c = Civil::from_epoch(secs);
    let months = c.y.checked_mul(12)?.checked_add(i64::from(c.m) - 1)?.checked_add(n)?;
    let (y, m) = (months.div_euclid(12), (months.rem_euclid(12) + 1) as u32);
    let d = c.d.min(last_day(y, m));
    Some(Civil { y, m, d, ..c }.checked()?.to_epoch())
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
/// use kevy_time::{eval, Civil};
/// let jan31 = Civil::from_date(2023, 1, 31).ok_or("not a date")?.to_epoch();
/// let c = Civil::from_epoch(eval(b"@now+1mo", jan31).ok_or("no bound")?);
/// assert_eq!((c.month(), c.day()), (2, 28));
/// # Ok::<(), &str>(())
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
            // Checked, like the five above it. These two were not, so
            // the same expression was refused in days and silently wrong
            // in months.
            b"mo" => checked_add_months(now, sign.checked_mul(n)?),
            b"y" => checked_add_months(now, sign.checked_mul(n.checked_mul(12)?)?),
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
    let (h, min, s) = match time {
        None => (0, 0, 0),
        Some(t) => {
            if t[2] != b':' || t[5] != b':' {
                return None;
            }
            (num(&t[..2])? as u32, num(&t[3..5])? as u32, num(&t[6..8])? as u32)
        }
    };
    Some(Civil::from_date(y, m, d)?.with_time(h, min, s)?.to_epoch())
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
