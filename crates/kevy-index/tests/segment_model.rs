//! Random operation sequences against a naive model: a plain map from key
//! to value, filtered and sorted at query time. The model shares no code
//! or layout with the segment, so a defect in one is not hidden by the
//! same defect in the other.

use std::collections::BTreeMap;

use kevy_index::{Cursor, IndexValue, Segment};

#[derive(Default)]
struct Model {
    rows: BTreeMap<Vec<u8>, IndexValue>,
    coerce_failures: u64,
}

impl Model {
    fn sorted(&self) -> Vec<(IndexValue, Vec<u8>)> {
        let mut all: Vec<_> = self.rows.iter().map(|(k, v)| (v.clone(), k.clone())).collect();
        all.sort();
        all
    }

    fn duplicates(&self) -> u64 {
        let mut per_value: BTreeMap<&IndexValue, u32> = BTreeMap::new();
        for v in self.rows.values() {
            *per_value.entry(v).or_default() += 1;
        }
        per_value.values().filter(|&&n| n > 1).count() as u64
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn key_space() -> Vec<Vec<u8>> {
    let mut keys: Vec<Vec<u8>> = (0..24).map(|i| format!("k{i}").into_bytes()).collect();
    keys.push(Vec::new());
    keys.push(vec![0xFF; 80]);
    keys.push(vec![0x00; 3]);
    keys
}

fn value(rng: &mut Rng, strings: bool) -> IndexValue {
    let n = rng.below(10);
    match strings {
        true => IndexValue::Str(format!("v{n}").into_bytes()),
        false => IndexValue::I64(n as i64 - 3),
    }
}

fn check_books(seg: &Segment, model: &Model, keys: &[Vec<u8>]) {
    let st = seg.stats();
    assert_eq!(st.entries, model.rows.len() as u64, "entries");
    assert_eq!(st.duplicates, model.duplicates(), "duplicates");
    assert_eq!(st.coerce_failures, model.coerce_failures, "coerce failures");
    for k in keys {
        assert_eq!(seg.verify_entry(k), model.rows.get(k), "verify_entry {k:?}");
    }
    let mut seen = Vec::new();
    seg.each_entry(|k, v| seen.push((k.to_vec(), v.clone())));
    seen.sort();
    let want: Vec<_> = model.rows.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    assert_eq!(seen, want, "each_entry");
    let scanned: Vec<_> = seg.scan(None, false).map(|(v, k)| (v.clone(), k.to_vec())).collect();
    assert_eq!(scanned, model.sorted(), "the tree and the reverse side hold the same rows");
    assert_eq!(seg.max_value(), model.sorted().last().map(|(v, _)| v));
}

fn check_range(seg: &Segment, model: &Model, lo: &IndexValue, hi: &IndexValue, page: usize) {
    let want: Vec<_> = model
        .sorted()
        .into_iter()
        .filter(|(v, _)| v >= lo && v <= hi)
        .map(|(v, k)| (k, v))
        .collect();
    let mut got = Vec::new();
    let mut cursor: Option<Cursor> = None;
    loop {
        let (hits, next) = seg.range(lo, hi, cursor.as_ref(), page);
        got.extend(hits);
        match next {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    assert_eq!(got, want, "range [{lo:?}, {hi:?}] by pages of {page}");
    assert_eq!(seg.count(lo, hi), want.len() as u64, "count");
    let eq_want: Vec<_> = want.iter().filter(|(_, v)| v == lo).map(|(k, _)| k.clone()).collect();
    assert_eq!(seg.eq(lo, usize::MAX), eq_want, "eq {lo:?}");
}

fn check_scans(seg: &Segment, model: &Model, rng: &mut Rng) {
    let all = model.sorted();
    let Some((v, k)) = all.get(rng.below(all.len() as u64 + 1) as usize) else { return };
    let after = Cursor { value: v.clone(), key: k.clone() };
    let up: Vec<_> = seg.scan(Some(&after), false).map(|(v, k)| (v.clone(), k.to_vec())).collect();
    let want_up: Vec<_> = all.iter().filter(|e| (&e.0, &e.1) > (v, k)).cloned().collect();
    assert_eq!(up, want_up, "ascending scan past {after:?}");
    let down: Vec<_> = seg.scan(Some(&after), true).map(|(v, k)| (v.clone(), k.to_vec())).collect();
    let want_down: Vec<_> = all.iter().rev().filter(|e| (&e.0, &e.1) < (v, k)).cloned().collect();
    assert_eq!(down, want_down, "descending scan past {after:?}");
    let full_down: Vec<_> = seg.scan(None, true).map(|(v, k)| (v.clone(), k.to_vec())).collect();
    assert_eq!(full_down, all.iter().rev().cloned().collect::<Vec<_>>(), "descending scan");
}

fn step(seg: &mut Segment, model: &mut Model, rng: &mut Rng, keys: &[Vec<u8>], strings: bool) {
    let k = keys[rng.below(keys.len() as u64) as usize].clone();
    match rng.below(10) {
        0..=3 => {
            let v = value(rng, strings);
            seg.apply(&k, Some(v.clone()));
            model.rows.insert(k, v);
        }
        4 => {
            if let Some(v) = model.rows.get(&k).cloned() {
                seg.apply(&k, Some(v));
            }
        }
        5 => {
            seg.apply(&k, None);
            model.rows.remove(&k);
            model.coerce_failures += 1;
        }
        6 => {
            seg.remove(&k);
            model.rows.remove(&k);
        }
        7 => {
            let bound = value(rng, strings);
            let preview: Vec<_> =
                seg.iter_below(&bound).map(|(v, k)| (v.clone(), k.to_vec())).collect();
            let got = seg.split_off_below(&bound);
            let want: Vec<_> = model.sorted().into_iter().filter(|(v, _)| *v < bound).collect();
            model.rows.retain(|_, v| *v >= bound);
            assert_eq!(preview, want, "iter_below {bound:?}");
            assert_eq!(got, want, "split_off_below {bound:?}");
        }
        _ => {
            let (a, b) = (value(rng, strings), value(rng, strings));
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            check_range(seg, model, &lo, &hi, 1 + rng.below(4) as usize);
            check_scans(seg, model, rng);
        }
    }
}

fn run(seed: u64, strings: bool) {
    let keys = key_space();
    let mut rng = Rng(seed);
    let mut seg = Segment::new();
    let mut model = Model::default();
    for _ in 0..1500 {
        step(&mut seg, &mut model, &mut rng, &keys, strings);
        check_books(&seg, &model, &keys);
    }
}

#[test]
fn i64_segments_follow_the_model() {
    for seed in 1..=12 {
        run(seed, false);
    }
}

#[test]
fn str_segments_follow_the_model() {
    for seed in 101..=112 {
        run(seed, true);
    }
}
