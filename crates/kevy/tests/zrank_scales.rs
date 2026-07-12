//! ZRANK must be O(log N), and the whole rank/range family must stay
//! correct on the order-statistic tree. Three faces:
//!
//! 1. a correctness sweep against a sorted-Vec oracle (10k members,
//!    random scores, then a 3k-member ZREM churn),
//! 2. a scaling assertion — the per-call cost of ZRANK on a 64k-member
//!    set may not be a large multiple of the 1k-member cost (the old
//!    linear-scan code measured a ~64× ratio; log-factor code measures
//!    ~1×; the bar is 8× so a loaded machine can never flake it),
//! 3. ZPOPMIN.BELOW keeps its exact delayed-job semantics (strictly
//!    below, count-capped, ascending pop order).

use std::time::Instant;

use kevy_store::{ScoreBound, Store};

/// Deterministic splitmix64 — replayable member/score streams.
struct SplitMix(u64);
impl SplitMix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

fn member(i: u64) -> Vec<u8> {
    format!("member:{i:08}").into_bytes()
}

/// Build a zset of `n` members with pseudorandom scores; returns the
/// oracle: `(member, score)` sorted by `(score, member)`.
fn fill(store: &mut Store, key: &[u8], n: u64, rng: &mut SplitMix) -> Vec<(Vec<u8>, f64)> {
    let mut oracle = Vec::with_capacity(n as usize);
    for i in 0..n {
        let score = (rng.next() % 1_000_000) as f64 / 10.0;
        let m = member(i);
        store.zadd(key, &[(score, m.as_slice())]).expect("zadd");
        oracle.push((m, score));
    }
    oracle.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    oracle
}

fn assert_agrees(store: &mut Store, key: &[u8], oracle: &[(Vec<u8>, f64)], rng: &mut SplitMix) {
    // 500 random members: zrank == oracle position.
    for _ in 0..500 {
        let pick = (rng.next() % oracle.len() as u64) as usize;
        let (m, _) = &oracle[pick];
        let got = store.zrank(key, m).expect("zrank");
        assert_eq!(got, Some(pick), "zrank({})", String::from_utf8_lossy(m));
    }
    assert_eq!(store.zrank(key, b"member:no-such").expect("zrank"), None);
    // zrange over random windows == oracle slices.
    for _ in 0..50 {
        let a = (rng.next() % oracle.len() as u64) as i64;
        let b = (rng.next() % oracle.len() as u64) as i64;
        let (start, stop) = (a.min(b), a.max(b));
        let got = store.zrange(key, start, stop).expect("zrange");
        let want = &oracle[start as usize..=stop as usize];
        assert_eq!(got, want, "zrange({start}, {stop})");
    }
    // zcount over random score windows == oracle filter count.
    for _ in 0..50 {
        let a = (rng.next() % 1_000_000) as f64 / 10.0;
        let b = (rng.next() % 1_000_000) as f64 / 10.0;
        let (lo, hi) = (a.min(b), a.max(b));
        let got = store
            .zcount(
                key,
                ScoreBound { value: lo, exclusive: false },
                ScoreBound { value: hi, exclusive: true },
            )
            .expect("zcount");
        let want = oracle.iter().filter(|(_, s)| *s >= lo && *s < hi).count();
        assert_eq!(got, want, "zcount([{lo}, {hi}))");
    }
}

#[test]
fn rank_range_count_agree_with_oracle_through_churn() {
    let mut rng = SplitMix(0x5EED_0001);
    let mut store = Store::new();
    let key = b"lb";
    let mut oracle = fill(&mut store, key, 10_000, &mut rng);
    assert_agrees(&mut store, key, &oracle, &mut rng);

    // Remove 3k members, then everything must still agree.
    for _ in 0..3_000 {
        let pick = (rng.next() % oracle.len() as u64) as usize;
        let (m, _) = oracle.remove(pick);
        assert_eq!(store.zrem(key, &[m.as_slice()]).expect("zrem"), 1);
    }
    assert_eq!(store.zcard(key).expect("zcard"), oracle.len());
    assert_agrees(&mut store, key, &oracle, &mut rng);
}

/// Time `calls` ZRANK hits spread across the whole set; ns per call.
fn zrank_ns_per_call(store: &mut Store, key: &[u8], n: u64, calls: u64) -> f64 {
    // Warm once so first-touch page faults are off the clock.
    let probe = member(n / 2);
    store.zrank(key, &probe).expect("zrank").expect("present");
    let start = Instant::now();
    let mut sink = 0usize;
    for c in 0..calls {
        let m = member((c.wrapping_mul(0x9E37_79B9) ^ c) % n);
        sink += store.zrank(key, &m).expect("zrank").expect("present");
    }
    let dt = start.elapsed().as_nanos() as f64 / calls as f64;
    assert!(sink > 0, "keep the loop observable");
    dt
}

#[test]
fn zrank_cost_does_not_scale_linearly() {
    let mut rng = SplitMix(0x5EED_0002);
    let mut store = Store::new();
    fill(&mut store, b"small", 1_000, &mut rng);
    fill(&mut store, b"big", 64_000, &mut rng);

    // Median of 3 runs each, so one scheduler hiccup cannot skew a side.
    let mut small = [0.0f64; 3];
    let mut big = [0.0f64; 3];
    for i in 0..3 {
        small[i] = zrank_ns_per_call(&mut store, b"small", 1_000, 2_000);
        big[i] = zrank_ns_per_call(&mut store, b"big", 64_000, 2_000);
    }
    small.sort_by(f64::total_cmp);
    big.sort_by(f64::total_cmp);
    let ratio = big[1] / small[1];
    eprintln!(
        "zrank per-call: 1k set {:.0} ns, 64k set {:.0} ns, ratio {ratio:.2}x",
        small[1], big[1]
    );
    // 64× the members. Linear ZRANK ⇒ ~64× the time (the old code
    // measured ≳30× under this harness); O(log N) ⇒ ~1.7× at most
    // (log ratio + cache effects). 8× is the generous fail line.
    assert!(
        ratio < 8.0,
        "ZRANK per-call cost scaled by {ratio:.1}× from 1k to 64k members \
         (small {:.0} ns, big {:.0} ns) — that is a linear scan, not O(log N)",
        small[1],
        big[1]
    );
}

#[test]
fn zpopmin_below_keeps_exact_semantics() {
    let mut store = Store::new();
    let pairs: &[(f64, &[u8])] =
        &[(10.0, b"j1"), (20.0, b"j2"), (30.0, b"j3"), (99.0, b"j4")];
    store.zadd(b"delayed", pairs).expect("zadd");

    // Strictly below: the threshold member itself must NOT pop.
    let got = store.zpopmin_below(b"delayed", 20.0, 10).expect("pop");
    assert_eq!(got, vec![(b"j1".to_vec(), 10.0)]);

    // count caps the due set, ascending order.
    let got = store.zpopmin_below(b"delayed", 100.0, 2).expect("pop");
    assert_eq!(got, vec![(b"j2".to_vec(), 20.0), (b"j3".to_vec(), 30.0)]);

    // Nothing due ⇒ empty, set untouched.
    let got = store.zpopmin_below(b"delayed", 50.0, 10).expect("pop");
    assert_eq!(got, Vec::new());
    assert_eq!(store.zcard(b"delayed").expect("zcard"), 1);

    // Drain the rest; the key disappears when emptied.
    let got = store.zpopmin_below(b"delayed", 1_000.0, 10).expect("pop");
    assert_eq!(got, vec![(b"j4".to_vec(), 99.0)]);
    assert_eq!(store.zcard(b"delayed").expect("zcard"), 0);
}
