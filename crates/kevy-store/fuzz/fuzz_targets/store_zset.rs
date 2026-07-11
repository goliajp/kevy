//! Fuzz `kevy_store` sorted sets against a `BTreeMap<member, score>`
//! oracle ordered by `(score.total_cmp, member)`.
//!
//! The risky machinery is the dual encoding: zsets start as the inline
//! `SmallZSetInline` (≤ 2 members, each ≤ 13 bytes) and promote to the
//! heap `ZSetData` on the third member or the first long member. Member
//! lengths here run 0..=19 bytes and op streams add/remove across the
//! count boundary, so promotion fires constantly and both encodings +
//! the switch itself are compared against the same oracle. Invariants:
//!
//!   * zadd/zadd_borrowed added-count, zrem removed-count, zscore,
//!     zcard, zincrby return value, zrank all agree with the oracle
//!   * zrange (rank form, Redis negative-index semantics re-derived
//!     independently) matches the oracle's `(score, member)` ordering
//!   * zrange_by_score / zcount with inclusive/exclusive/±inf bounds
//!     match an oracle filter
//!   * zpopmin pops exactly the oracle's k lowest `(score, member)` pairs
//!   * an emptied zset key reads back as absent (zcard 0, empty ranges)
//!
//! Scores are generated as i16/8.0 — always finite, never NaN and never
//! -0.0 — matching the server dispatch layer which rejects non-numeric
//! score input before the store is reached.

#![no_main]

use kevy_store::{ScoreBound, Store};
use libfuzzer_sys::fuzz_target;
use std::collections::BTreeMap;

struct Input<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Input<'a> {
    fn byte(&mut self) -> Option<u8> {
        let b = *self.data.get(self.pos)?;
        self.pos += 1;
        Some(b)
    }

    fn score(&mut self) -> Option<f64> {
        let a = self.byte()?;
        let b = self.byte()?;
        Some(f64::from(i16::from_le_bytes([a, b])) / 8.0)
    }

    /// Member 0..=19 bytes: crosses the 13-byte inline-member cap.
    fn member(&mut self) -> Option<Vec<u8>> {
        let len = (self.byte()? % 20) as usize;
        let end = (self.pos + len).min(self.data.len());
        let m = self.data[self.pos..end].to_vec();
        self.pos = end;
        Some(m)
    }

    fn bound(&mut self) -> Option<ScoreBound> {
        let kind = self.byte()?;
        let value = match kind % 4 {
            0 => f64::NEG_INFINITY,
            1 => f64::INFINITY,
            _ => self.score()?,
        };
        Some(ScoreBound { value, exclusive: kind & 0x10 != 0 })
    }
}

type Oracle = BTreeMap<Vec<u8>, f64>;

/// The oracle's ZRANGE ordering: `(score, member)` ascending, scores by
/// total order (never NaN here).
fn sorted(oracle: &Oracle) -> Vec<(Vec<u8>, f64)> {
    let mut v: Vec<(Vec<u8>, f64)> = oracle.iter().map(|(m, s)| (m.clone(), *s)).collect();
    v.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    v
}

/// Redis rank-range semantics, independently re-derived (negative index
/// = from the end; clamp; empty when start > stop after normalization).
fn rank_range(len: usize, start: i64, stop: i64) -> Option<(usize, usize)> {
    if len == 0 {
        return None;
    }
    let len = len as i64;
    let s = (if start < 0 { start + len } else { start }).max(0);
    let e = (if stop < 0 { stop + len } else { stop }).min(len - 1);
    if s > e || s >= len { None } else { Some((s as usize, e as usize)) }
}

fuzz_target!(|data: &[u8]| {
    let mut input = Input { data, pos: 0 };
    let mut store = Store::new();
    // Two keys so cross-key isolation is also on the table.
    let keys: [&[u8]; 2] = [b"zk0", b"zk1"];
    let mut oracles: [Oracle; 2] = [Oracle::new(), Oracle::new()];

    while let Some(op) = input.byte() {
        let ki = (op >> 6) as usize & 1;
        let key = keys[ki];
        let oracle = &mut oracles[ki];
        match op % 11 {
            0 | 1 => {
                let (Some(score), Some(member)) = (input.score(), input.member()) else {
                    break;
                };
                let want_added = usize::from(!oracle.contains_key(&member));
                oracle.insert(member.clone(), score);
                let added = store.zadd(key, &[(score, member.as_slice())]).expect("zadd");
                assert_eq!(added, want_added, "zadd added-count diverged");
            }
            2 => {
                // Batch zadd, duplicate members allowed —
                // the store applies pairs in order, so does the oracle.
                let n = (input.byte().unwrap_or(0) % 5) as usize;
                let mut pairs: Vec<(f64, Vec<u8>)> = Vec::new();
                for _ in 0..n {
                    let (Some(s), Some(m)) = (input.score(), input.member()) else { break };
                    pairs.push((s, m));
                }
                let mut want_added = 0usize;
                for (s, m) in &pairs {
                    if oracle.insert(m.clone(), *s).is_none() {
                        want_added += 1;
                    }
                }
                let borrowed: Vec<(f64, &[u8])> =
                    pairs.iter().map(|(s, m)| (*s, m.as_slice())).collect();
                let added = store.zadd(key, &borrowed).expect("zadd");
                assert_eq!(added, want_added, "batch zadd added-count diverged");
            }
            3 => {
                let Some(member) = input.member() else { break };
                let want = usize::from(oracle.remove(&member).is_some());
                let removed = store.zrem(key, &[member.as_slice()]).expect("zrem");
                assert_eq!(removed, want, "zrem removed-count diverged");
            }
            4 => {
                let Some(member) = input.member() else { break };
                assert_eq!(
                    store.zscore(key, &member).expect("zscore"),
                    oracle.get(&member).copied(),
                    "zscore diverged"
                );
            }
            5 => {
                let (Some(incr), Some(member)) = (input.score(), input.member()) else {
                    break;
                };
                let next = oracle.get(&member).copied().unwrap_or(0.0) + incr;
                oracle.insert(member.clone(), next);
                let got = store.zincrby(key, incr, &member).expect("zincrby");
                assert_eq!(got, next, "zincrby return diverged");
            }
            6 => {
                let Some(member) = input.member() else { break };
                let want = sorted(oracle).iter().position(|(m, _)| *m == member);
                assert_eq!(
                    store.zrank(key, &member).expect("zrank"),
                    want,
                    "zrank diverged"
                );
            }
            7 => {
                let (Some(a), Some(b)) = (input.byte(), input.byte()) else { break };
                // Small signed ranks around the set size, both signs.
                let start = i64::from(a as i8);
                let stop = i64::from(b as i8);
                let all = sorted(oracle);
                let want: Vec<(Vec<u8>, f64)> = match rank_range(all.len(), start, stop) {
                    None => Vec::new(),
                    Some((s, e)) => all[s..=e].to_vec(),
                };
                let got = store.zrange(key, start, stop).expect("zrange");
                assert_eq!(got, want, "zrange diverged (start {start} stop {stop})");
            }
            8 => {
                let (Some(min), Some(max)) = (input.bound(), input.bound()) else { break };
                let ge = |s: f64| if min.exclusive { s > min.value } else { s >= min.value };
                let le = |s: f64| if max.exclusive { s < max.value } else { s <= max.value };
                let want: Vec<(Vec<u8>, f64)> = sorted(oracle)
                    .into_iter()
                    .filter(|(_, s)| ge(*s) && le(*s))
                    .collect();
                let want_count = want.len();
                let got = store
                    .zrange_by_score(
                        key,
                        ScoreBound { value: min.value, exclusive: min.exclusive },
                        ScoreBound { value: max.value, exclusive: max.exclusive },
                    )
                    .expect("zrange_by_score");
                assert_eq!(got, want, "zrange_by_score diverged");
                let count = store
                    .zcount(
                        key,
                        ScoreBound { value: min.value, exclusive: min.exclusive },
                        ScoreBound { value: max.value, exclusive: max.exclusive },
                    )
                    .expect("zcount");
                assert_eq!(count, want_count, "zcount diverged");
            }
            9 => {
                let count = (input.byte().unwrap_or(1) % 4) as usize;
                let all = sorted(oracle);
                let want: Vec<(Vec<u8>, f64)> = all[..count.min(all.len())].to_vec();
                for (m, _) in &want {
                    oracle.remove(m);
                }
                let got = store.zpopmin(key, count).expect("zpopmin");
                assert_eq!(got, want, "zpopmin diverged");
            }
            10 => {
                assert_eq!(store.zcard(key).expect("zcard"), oracle.len(), "zcard diverged");
                let got = store.zrange(key, 0, -1).expect("zrange full");
                assert_eq!(got, sorted(oracle), "full zrange diverged");
            }
            _ => unreachable!(),
        }
    }

    // Final full-state check on both keys, both encodings.
    for (ki, key) in keys.iter().enumerate() {
        assert_eq!(store.zcard(key).expect("zcard"), oracles[ki].len());
        assert_eq!(store.zrange(key, 0, -1).expect("zrange"), sorted(&oracles[ki]));
    }
});
