//! [`super`]'s tests: the civil round-trip property over a wide day
//! sweep, the leap rules, month-end clamping, and the @-expression
//! grammar including its refusals.

use super::*;

#[test]
fn civil_round_trips_over_a_million_days() {
    // ±500k days around the epoch (~±1370 years), plus second-level
    // offsets at the edges of a day.
    for day in (-500_000..500_000).step_by(97) {
        for off in [0i64, 1, 43_199, 86_399] {
            let secs = day * 86_400 + off;
            let c = civil_from_epoch(secs);
            assert_eq!(epoch_from_civil(c), secs, "drift at {secs} ({c:?})");
            assert!((1..=12).contains(&c.m) && c.d >= 1 && c.d <= 31);
        }
    }
}

#[test]
fn known_dates_pin_the_calendar() {
    // The epoch itself, and the leap rules: 2000 (div-400 leap),
    // 1900 (century non-leap), 2024 (plain leap).
    assert_eq!(epoch_from_civil(Civil { y: 1970, m: 1, d: 1, h: 0, min: 0, s: 0 }), 0);
    assert_eq!(civil_from_epoch(951_782_400), Civil { y: 2000, m: 2, d: 29, h: 0, min: 0, s: 0 });
    assert_eq!(eval(b"@1900-02-29", 0), None, "1900 was not a leap year");
    assert!(eval(b"@2024-02-29", 0).is_some());
    // A negative epoch decodes correctly.
    assert_eq!(civil_from_epoch(-86_400).d, 31);
    assert_eq!(civil_from_epoch(-86_400).y, 1969);
}

#[test]
fn add_months_clamps_month_ends() {
    let jan31 = epoch_from_civil(Civil { y: 2026, m: 1, d: 31, h: 12, min: 0, s: 0 });
    assert_eq!(civil_from_epoch(add_months(jan31, 1)).d, 28, "2026-02 clamps to 28");
    let jan31_leap = epoch_from_civil(Civil { y: 2024, m: 1, d: 31, h: 0, min: 0, s: 0 });
    assert_eq!(civil_from_epoch(add_months(jan31_leap, 1)).d, 29, "2024-02 clamps to 29");
    // A year back and forth across the year boundary.
    let c = civil_from_epoch(add_months(jan31, -13));
    assert_eq!((c.y, c.m, c.d), (2024, 12, 31));
    // The time of day survives.
    assert_eq!(civil_from_epoch(add_months(jan31, 1)).h, 12);
}

#[test]
fn eval_speaks_the_whole_grammar() {
    let now = 1_754_000_000; // 2025-07-31T22:13:20 UTC
    assert_eq!(eval(b"@now", now), Some(now));
    assert_eq!(eval(b"@now-7d", now), Some(now - 7 * 86_400));
    assert_eq!(eval(b"@now+90s", now), Some(now + 90));
    assert_eq!(eval(b"@now-2w", now), Some(now - 14 * 86_400));
    assert_eq!(eval(b"@now+30m", now), Some(now + 1800));
    assert_eq!(eval(b"@now-6h", now), Some(now - 6 * 3600));
    assert_eq!(eval(b"@now-1mo", now), Some(add_months(now, -1)));
    assert_eq!(eval(b"@now+2y", now), Some(add_months(now, 24)));
    assert_eq!(
        eval(b"@2026-08-03", 0),
        Some(epoch_from_civil(Civil { y: 2026, m: 8, d: 3, h: 0, min: 0, s: 0 }))
    );
    assert_eq!(
        eval(b"@2026-08-03T09:15:30", 0),
        Some(epoch_from_civil(Civil { y: 2026, m: 8, d: 3, h: 9, min: 15, s: 30 }))
    );
}

#[test]
fn eval_refuses_every_malformed_shape() {
    for bad in [
        b"now".as_slice(),       // no @ sigil
        b"@later",               // unknown word
        b"@now-",                // sign, no digits
        b"@now-7",               // digits, no unit
        b"@now-7q",              // unknown unit
        b"@now*7d",              // unknown operator
        b"@2026-13-01",          // month 13
        b"@2026-02-30",          // day past month end
        b"@2026-8-3",            // unpadded
        b"@2026-08-03T25:00:00", // hour 25
        b"@2026-08-03 09:15:30", // space, not T
        b"@",                    // empty body
    ] {
        assert_eq!(eval(bad, 0), None, "{:?} must refuse", String::from_utf8_lossy(bad));
    }
}

/// A longer unit can never succeed where a shorter one overflowed.
///
/// `s`/`m`/`h`/`d`/`w` were checked and `mo`/`y` were not, so exactly
/// this was false: `@now+9223372036854775807d` was correctly refused
/// while `@now+9223372036854775807mo` — a strictly larger offset —
/// returned a timestamp in 1969 in release, and panicked in debug.
///
/// The value arrives straight from client argv as a query bound
/// (`IDX.QUERY … RANGE`), so a wrong answer is a wrong row set and a
/// panic is a shard going down.
#[test]
fn a_longer_unit_never_succeeds_where_a_shorter_one_overflowed() {
    // Ascending duration. `mo` and `y` are the two that were unchecked.
    const UNITS: [&str; 7] = ["s", "m", "h", "d", "w", "mo", "y"];
    let now = 1_757_000_000i64;
    let mut refusals = 0usize;

    for n in ["1", "1000000", "100000000000000", "9223372036854775807"] {
        for sign in ["+", "-"] {
            let mut refused_at = None;
            for (i, unit) in UNITS.iter().enumerate() {
                let e = format!("@now{sign}{n}{unit}");
                let got = eval(e.as_bytes(), now);
                match (refused_at, got) {
                    (Some(first), Some(v)) => panic!(
                        "{e} answered {v}, but the shorter unit {} already overflowed",
                        UNITS[first]
                    ),
                    (None, None) => {
                        refused_at = Some(i);
                        refusals += 1;
                    }
                    (Some(_), None) => refusals += 1,
                    (None, Some(_)) => {}
                }
            }
        }
    }
    // The floor: if nothing ever overflowed, the property above holds
    // vacuously and this test proves nothing.
    assert!(refusals > 0, "no offset overflowed — the matrix is too small to test anything");

    // And ordinary offsets still work, in the right direction and by a
    // sane amount, so "refuse everything" cannot pass.
    let a = eval(b"@now+1mo", now).expect("one month ahead");
    let b = eval(b"@now-1mo", now).expect("one month back");
    assert!(a > now && b < now, "a month either way must straddle now");
    assert!(a - now < 32 * 86_400 && now - b < 32 * 86_400, "a month is not a year");
    assert!(eval(b"@now+7d", now).is_some() && eval(b"@now+1y", now).is_some());
}
