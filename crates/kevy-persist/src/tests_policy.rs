//! `RewritePolicy::baseline_matters`: where open may skip the baseline
//! estimate, the growth-rule decision must not depend on the baseline.

use super::*;
use crate::tests::temp_file;

const MIB: u64 = 1024 * 1024;

fn policy(pct: u32, min_size: u64, interval_secs: u64) -> RewritePolicy {
    RewritePolicy { pct, min_size, bytes: 0, interval_secs }
}

#[test]
fn the_baseline_matters_only_near_min_size_or_with_the_staleness_rule() {
    let p = policy(100, 64 * MIB, 0);
    assert!(!p.baseline_matters(0));
    assert!(!p.baseline_matters(32 * MIB));
    assert!(p.baseline_matters(32 * MIB + 1));
    let p = policy(50, 30, 0);
    assert!(!p.baseline_matters(20));
    assert!(p.baseline_matters(21));
    assert!(!policy(0, 64 * MIB, 0).baseline_matters(u64::MAX));
    assert!(policy(100, 64 * MIB, 1).baseline_matters(0));
    assert!(policy(100, 0, 0).baseline_matters(1));
}

// every (len, estimate <= len, current size) the skip applies to decides
// the same with the baseline at `len` as at the estimate
#[test]
fn where_the_baseline_does_not_matter_it_decides_nothing() {
    let path = temp_file("aof-baseline-skip");
    let mut aof = Aof::open(&path, Fsync::No).unwrap();
    let mut checked = 0;
    for &(pct, min) in &[(100u32, 400u64), (50, 300), (25, 1000), (300, 800)] {
        let p = policy(pct, min, 0);
        for len in (8..=min).step_by(7) {
            if p.baseline_matters(len) {
                continue;
            }
            for est in (1..=len).step_by(5) {
                for cur in (len..=min * 3).step_by(11) {
                    aof.size_bytes = cur;
                    aof.size_at_last_rewrite = len;
                    let at_len = aof.rewrite_due(p);
                    aof.size_at_last_rewrite = est;
                    assert_eq!(
                        at_len,
                        aof.rewrite_due(p),
                        "pct {pct} min {min} len {len} est {est} cur {cur}"
                    );
                    checked += 1;
                }
            }
        }
    }
    assert!(checked > 10_000, "the sweep covered only {checked} cases");
}
