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
