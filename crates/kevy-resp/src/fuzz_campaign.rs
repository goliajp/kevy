//! Campaigns: many [`run_one`] calls folded into a [`Summary`].

use super::{FuzzOutcome, Lcg, Strategy, Summary, run_one};

/// Run N campaigns across all strategies. Returns counts + any
/// timeouts found.
///
/// ```
/// use kevy_resp::fuzz::run_n;
/// let s = run_n(100, 7);
/// assert_eq!(s.total, 100);
/// // the same base seed replays the same campaign
/// assert_eq!(s, run_n(100, 7));
/// ```
#[must_use]
pub fn run_n(n: u64, base_seed: u64) -> Summary {
    let mut summary = Summary::default();
    for i in 0..n {
        let seed = base_seed.wrapping_add(i);
        let strategy = Strategy::pick(&mut Lcg::new(seed.wrapping_mul(0xDEAD_BEEF_CAFE_F00D)));
        let r = run_one(strategy, seed);
        summary.total += 1;
        match r.outcome {
            FuzzOutcome::Parsed { .. } => summary.parsed += 1,
            FuzzOutcome::Incomplete => summary.incomplete += 1,
            FuzzOutcome::ParseError => summary.errored += 1,
            FuzzOutcome::Timeout { elapsed_micros } => {
                summary.timed_out.push((strategy, seed, elapsed_micros));
            }
        }
    }
    summary
}
