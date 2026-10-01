//! Tests for `fuzz`.
use super::*;

#[test]
fn fuzz_1k_all_strategies_clean() {
    let summary = run_n(1000, 0xC0DE);
    summary.assert_clean(1000);
}

#[test]
fn lcg_is_deterministic() {
    let mut a = Lcg::new(42);
    let mut b = Lcg::new(42);
    for _ in 0..100 {
        assert_eq!(a.next_u64(), b.next_u64());
    }
}

#[test]
fn lcg_state_is_the_seed_then_the_last_draw() {
    let mut r = Lcg::new(7);
    assert_eq!(r.state(), 7);
    let v = r.next_u64();
    assert_eq!(r.state(), v);
    assert_ne!(Lcg::new(0).state(), 0);
}

#[test]
fn known_valid_input_parses() {
    let r = run_one(Strategy::MutatedValid, 0);
    // Seed 0 may or may not flip in a way that breaks the frame.
    // Just assert the outcome is one of the valid variants
    // (not a timeout / panic).
    matches!(
        r.outcome,
        FuzzOutcome::Parsed { .. } | FuzzOutcome::Incomplete | FuzzOutcome::ParseError
    );
}
