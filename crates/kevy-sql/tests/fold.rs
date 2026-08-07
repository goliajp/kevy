//! The fold face against probe-shaped statements — the same SQL text
//! forms funcgate replays from `bench/funcgate-corpus/`. The pinned
//! clock is the corpus' own: 2025-06-15T12:00:00Z (probe 08 header).

use kevy_sql::fold_select;

/// 2025-06-15 12:00:00 UTC in epoch microseconds — the corpus clock.
const NOW: i64 = 1_749_988_800 * 1_000_000;

fn one(sql: &str) -> String {
    let f = fold_select(sql, NOW).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    assert_eq!(f.columns.len(), 1, "{sql}: expected one column");
    let v = &f.columns[0];
    if v.is_null() { "NULL".into() } else { v.render() }
}

#[test]
fn scalar_calls_fold() {
    assert_eq!(one("SELECT trim('  hello  ')"), "hello");
    assert_eq!(one("SELECT split_part('a,b,c', ',', 2)"), "b");
    assert_eq!(one("SELECT floor(-1.5)"), "-2");
    assert_eq!(one("SELECT round(2.5)"), "3");
    assert_eq!(one("SELECT power(2, 10)"), "1024");
    assert_eq!(one("SELECT coalesce(NULL, 'x')"), "x");
    assert_eq!(one("SELECT nullif('a', 'a')"), "NULL");
    assert_eq!(one("SELECT lower('MiXeD')"), "mixed");
    assert_eq!(one("SELECT concat('a', NULL, 'b', 7)"), "ab7");
    assert_eq!(one("SELECT length(trim('  hi  '))"), "2"); // nesting
}

#[test]
fn casts_intervals_and_arithmetic() {
    assert_eq!(
        one("SELECT '2024-01-01 00:00:00'::TIMESTAMP + INTERVAL '90 minutes'"),
        "2024-01-01 01:30:00"
    );
    assert_eq!(
        one("SELECT INTERVAL '30 seconds' + '2024-01-01 00:00:00'::TIMESTAMP"),
        "2024-01-01 00:00:30"
    );
    assert_eq!(one("SELECT '2024-06-01'::DATE + INTERVAL '7 days'"), "2024-06-08 00:00:00");
    assert_eq!(one("SELECT INTERVAL '1 year 2 months'"), "1 year 2 mons");
    assert_eq!(one("SELECT INTERVAL '-3 days'"), "-3 days");
    // Month clamp: Jan 31 + 1 mon lands on the last day of Feb.
    assert_eq!(
        one("SELECT '2024-01-31'::DATE + INTERVAL '1 month'"),
        "2024-02-29 00:00:00"
    );
    assert_eq!(one("SELECT 1 + 2 * 3"), "7");
    assert_eq!(one("SELECT (1 + 2) * 3"), "9");
}

#[test]
fn extract_position_and_clock_forms() {
    assert_eq!(one("SELECT EXTRACT(YEAR FROM '2024-03-15 10:00:00'::TIMESTAMP)"), "2024");
    assert_eq!(one("SELECT date_part('hour', '2030-06-15'::DATE)"), "0");
    assert_eq!(one("SELECT position('ig' IN 'high')"), "2");
    // The clock rewrites: keyword form and call form, both pinned.
    assert_eq!(one("SELECT now()"), "2025-06-15 12:00:00");
    assert_eq!(one("SELECT CURRENT_DATE"), "2025-06-15");
    assert_eq!(one("SELECT date_trunc('month', now())"), "2025-06-01 00:00:00");
}

#[test]
fn refusals_are_named() {
    // FROM routes to the plan face, by name.
    let e = fold_select("SELECT 1 FROM users", 0).unwrap_err();
    assert!(e.message.contains("FROM"), "got: {}", e.message);
    // A column reference is refused at the reference itself, with the
    // same routing hint.
    let e = fold_select("SELECT lower(name) FROM users", 0).unwrap_err();
    assert!(e.message.contains("plan face"), "got: {}", e.message);
    // Unknown functions carry their name verbatim.
    let e = fold_select("SELECT no_such_fn(1)", 0).unwrap_err();
    assert!(e.message.contains("no_such_fn"), "got: {}", e.message);
    // PG-errors surface as errors, not values (probe 41's n=0).
    let e = fold_select("SELECT split_part('a,b', ',', 0)", 0).unwrap_err();
    assert!(e.message.contains("field position"), "got: {}", e.message);
}
