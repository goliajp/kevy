//! `Score`'s `Eq` must agree with its `Ord`.
//!
//! Split out of `value.rs` rather than left in it: that file reached 491 of
//! the 500-line ceiling with these tests inside, and this crate already
//! keeps its test modules in their own `tests_*.rs` files.

use crate::value::Score;
use core::cmp::Ordering;

/// `Eq` and `Ord` must agree, which is what Rust requires of a key in an
/// ordered container and what a derived `PartialEq` does not give here.
/// `-0.0` is the case that matters: `ZADD z -0 m` is accepted, and a
/// sorted set really does order `-0.0` before `0.0`.
///
/// `assert!` rather than `assert_eq!` because `Score` carries no `Debug`
/// and a hot key type should not grow one to please a test.
#[test]
fn eq_agrees_with_ord() {
    let interesting = [
        0.0_f64,
        -0.0,
        1.0,
        -1.0,
        f64::MIN,
        f64::MAX,
        f64::INFINITY,
        f64::NEG_INFINITY,
        1e-308,
        -1e-308,
    ];
    for &a in &interesting {
        for &b in &interesting {
            let (sa, sb) = (Score(a), Score(b));
            assert!(
                (sa == sb) == (sa.cmp(&sb) == Ordering::Equal),
                "Eq and Ord disagree on {a} vs {b}",
            );
        }
    }
}

/// The specific pair that made this worth writing down: equal under
/// `f64`'s `==`, distinct under the order the rank tree keys on.
#[test]
fn negative_zero_is_not_positive_zero() {
    assert!(Score(-0.0) != Score(0.0), "-0.0 must not equal 0.0");
    assert!(Score(-0.0).cmp(&Score(0.0)) == Ordering::Less);
    assert!(Score(-0.0) == Score(-0.0));
    // The point of the assertion above: plain `-0.0 == 0.0` is true, so a
    // derived PartialEq would have called these two the same score.
    // Not written as an assertion, because clippy is right that
    // asserting a constant tests nothing.
}
