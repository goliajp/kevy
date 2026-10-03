//! `zrange_select` against the plain reading of each span: sort, filter,
//! reverse, then skip and take — in every encoding a sorted set has.

use alloc::vec::Vec;

use crate::{LexBound, ScoreBound, Store, Value, ZSpan};

type Entry = (Vec<u8>, f64);

struct Rng(u64);
impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

fn rank_model(sorted: &[Entry], a: i64, b: i64) -> Vec<Entry> {
    let n = sorted.len() as i64;
    let s = if a < 0 { (a + n).max(0) } else { a };
    let e = if b < 0 { b + n } else { b.min(n - 1) };
    if n == 0 || s > e || s >= n { Vec::new() } else { sorted[s as usize..=e as usize].to_vec() }
}

fn limit_model(v: Vec<Entry>, limit: Option<(i64, i64)>) -> Vec<Entry> {
    match limit {
        None => v,
        Some((off, _)) if off < 0 => Vec::new(),
        Some((off, cnt)) => {
            let it = v.into_iter().skip(off as usize);
            if cnt < 0 { it.collect() } else { it.take(cnt as usize).collect() }
        }
    }
}

fn score_bound(rng: &mut Rng) -> ScoreBound {
    let v = match rng.below(9) {
        0 => f64::NEG_INFINITY,
        1 => f64::INFINITY,
        _ => rng.below(60) as f64 / 2.0,
    };
    if rng.below(2) == 0 { ScoreBound::inclusive(v) } else { ScoreBound::exclusive(v) }
}

fn lex_bound(rng: &mut Rng) -> LexBound {
    let m = alloc::format!("m{:03}", rng.below(400)).into_bytes();
    match rng.below(8) {
        0 => LexBound::NegInf,
        1 => LexBound::PosInf,
        2..=4 => LexBound::Inclusive(m),
        _ => LexBound::Exclusive(m),
    }
}

/// One random read of `key`, checked against `sorted`; lex reads only
/// when every score is the same, as Redis defines them.
fn check_one(s: &mut Store, sorted: &[Entry], one_score: bool, rng: &mut Rng) {
    let rev = rng.below(2) == 0;
    let n = sorted.len() as i64 + 3;
    let limit = (rng.below(2) == 0).then(|| (rng.below(8) as i64 - 1, rng.below(30) as i64 - 3));
    let ordered: Vec<Entry> =
        if rev { sorted.iter().rev().cloned().collect() } else { sorted.to_vec() };
    let (min_l, max_l) = (lex_bound(rng), lex_bound(rng));
    let (span, want) = match rng.below(if one_score { 3 } else { 2 }) {
        0 => {
            let (a, b) = (rng.below(2 * n as u64) as i64 - n, rng.below(2 * n as u64) as i64 - n);
            (ZSpan::Rank(a, b), rank_model(&ordered, a, b))
        }
        1 => {
            let (min, max) = (score_bound(rng), score_bound(rng));
            let hit: Vec<Entry> =
                ordered.iter().filter(|e| min.ge_ok(e.1) && max.le_ok(e.1)).cloned().collect();
            (ZSpan::Score(min, max), limit_model(hit, limit))
        }
        _ => {
            let hit: Vec<Entry> = ordered
                .iter()
                .filter(|e| {
                    min_l.as_end().admits_from_below(&e.0) && max_l.as_end().admits_from_above(&e.0)
                })
                .cloned()
                .collect();
            (ZSpan::Lex(min_l.as_end(), max_l.as_end()), limit_model(hit, limit))
        }
    };
    let limit = if matches!(span, ZSpan::Rank(..)) { None } else { limit };
    let r = s.zrange_select(b"z", span, rev, limit).expect("a zset");
    assert_eq!(r.len(), want.len(), "{span:?} rev {rev} limit {limit:?}");
    let got: Vec<Entry> = r.map(|(m, sc)| (m.to_vec(), sc)).collect();
    assert_eq!(got, want, "{span:?} rev {rev} limit {limit:?}");
}

fn run(members: u64, one_score: bool, reads: usize, encoding: impl Fn(&Value) -> bool) {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ members);
    let mut s = Store::new();
    let mut sorted: Vec<Entry> = Vec::new();
    for i in 0..members {
        let sc = if one_score { 0.0 } else { rng.below(60) as f64 / 2.0 };
        // two-byte members, so two of them fit the inline encoding
        let m = if members <= 2 {
            alloc::format!("m{i}")
        } else {
            alloc::format!("m{:03}", (i * 7919) % 1000)
        };
        let m = m.into_bytes();
        if s.zadd(b"z", &[(sc, m.as_slice())]).expect("zadd") == 1 {
            sorted.push((m, sc));
        }
    }
    sorted.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    s.snapshot_each(|_, v, _| assert!(encoding(v), "the encoding this run is for"));
    for _ in 0..reads {
        check_one(&mut s, &sorted, one_score, &mut rng);
    }
}

#[test]
fn selections_match_the_plain_reading_inline() {
    for n in 0..=2 {
        run(n, false, 300, |v| matches!(v, Value::SmallZSetInline(_)) || n == 0);
    }
}

#[test]
fn selections_match_the_plain_reading_flat() {
    let reads = if cfg!(miri) { 60 } else { 3000 };
    run(300, false, reads, |v| matches!(v, Value::ZSet(_)));
    run(300, true, reads, |v| matches!(v, Value::ZSet(_)));
}

#[test]
#[cfg_attr(
    miri,
    ignore = "a segmented set takes 16K members; the flat run covers the same code under miri"
)]
fn selections_match_the_plain_reading_segmented() {
    use crate::zset_seg::Z_PROMOTE;
    let mut rng = Rng(7);
    let mut s = Store::new();
    let mut sorted: Vec<Entry> = Vec::new();
    for one_score in [false, true] {
        s.del(&[b"z".as_slice()]);
        sorted.clear();
        for i in 0..(Z_PROMOTE as u64 + 3000) {
            let sc = if one_score { 0.0 } else { rng.below(60) as f64 / 2.0 };
            let m = alloc::format!("m{i:06}").into_bytes();
            s.zadd(b"z", &[(sc, m.as_slice())]).expect("zadd");
            sorted.push((m, sc));
        }
        sorted.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        s.snapshot_each(|_, v, _| assert!(matches!(v, Value::SegZSet(_))));
        for _ in 0..2000 {
            check_one(&mut s, &sorted, one_score, &mut rng);
        }
    }
}
