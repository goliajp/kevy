//! Random operation sequences against a naive model: a plain map from key
//! to `(value, stored values)`, filtered and sorted at query time. The
//! model shares no code or layout with the segment, so a defect in one is
//! not hidden by the same defect in the other. Every write passes the
//! row's old value from the model, the way the write path passes the row's
//! value before the write.

use std::collections::{BTreeMap, HashMap};

use kevy_index::{
    CompositeCol, Cursor, IndexKind, IndexSpec, IndexValue, ScalarClauses, Segment, SortOrder,
    ValType, ValueTest, composite_encode, order_key,
};

type Stored = Vec<Option<Vec<u8>>>;

#[derive(Default)]
struct Model {
    rows: BTreeMap<Vec<u8>, (IndexValue, Stored)>,
    coerce_failures: u64,
}

impl Model {
    fn sorted(&self) -> Vec<(IndexValue, Vec<u8>)> {
        let mut all: Vec<_> = self.rows.iter().map(|(k, (v, _))| (v.clone(), k.clone())).collect();
        all.sort();
        all
    }

    fn duplicates(&self) -> u64 {
        let mut per_value: BTreeMap<&IndexValue, u32> = BTreeMap::new();
        for (v, _) in self.rows.values() {
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

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    I64,
    F64,
    Str,
    Composite,
}

fn cols() -> Vec<CompositeCol> {
    vec![
        CompositeCol::new("s", ValType::Str),
        CompositeCol::new("t", ValType::I64).with_order(SortOrder::Desc),
    ]
}

/// Mostly `row:<digits>` keys; a few others turn up only after a while,
/// so a segment first packs digits and later has to widen.
fn key(rng: &mut Rng, step: usize) -> Vec<u8> {
    match rng.below(30) {
        0 if step > 300 => b"row:x".to_vec(),
        1 if step > 600 => format!("other:{}", rng.below(3)).into_bytes(),
        2 => format!("row:{}", "9".repeat(300 + rng.below(3) as usize)).into_bytes(),
        3 => b"row:".to_vec(),
        _ => format!("row:{}", rng.below(40)).into_bytes(),
    }
}

fn value(rng: &mut Rng, kind: Kind) -> IndexValue {
    let n = rng.below(10) as i64;
    match kind {
        Kind::I64 => IndexValue::I64(n - 3),
        Kind::F64 => IndexValue::F64((n as f64 - 3.0) / 2.0),
        Kind::Str => IndexValue::Str(match n {
            0 => Vec::new(),
            1 => b"a\0b".to_vec(),
            2 => vec![b'z'; 400],
            _ => format!("v{n}").into_bytes(),
        }),
        Kind::Composite => {
            let s = format!("s{}", n % 3);
            let t = format!("{}", n * 7);
            IndexValue::Str(
                composite_encode(&cols(), &[Some(s.as_bytes()), Some(t.as_bytes())])
                    .expect("coerces"),
            )
        }
    }
}

fn stored(rng: &mut Rng, arity: usize) -> Stored {
    (0..arity)
        .map(|_| match rng.below(6) {
            0 => None,
            1 => Some(Vec::new()),
            2 => Some(format!("{}", rng.below(100_000)).into_bytes()),
            3 => Some(b"007".to_vec()),
            4 => Some(vec![b'L'; 300]),
            _ => Some(format!("c{}", rng.below(3)).into_bytes()),
        })
        .collect()
}

fn refs(s: &Stored) -> Vec<Option<&[u8]>> {
    s.iter().map(|v| v.as_deref()).collect()
}

fn segment(kind: Kind, arity: usize, shaped: bool) -> Segment {
    if !shaped {
        return if arity == 0 { Segment::new() } else { Segment::with_values(arity) };
    }
    let ty = match kind {
        Kind::I64 => ValType::I64,
        Kind::F64 => ValType::F64,
        _ => ValType::Str,
    };
    let mut b = IndexSpec::builder("t.i", "row:", IndexKind::Range, ty).with_field("f");
    if kind == Kind::Composite {
        b = b.with_composite(cols());
    }
    if arity > 0 {
        b = b
            .with_values((0..arity).map(|i| kevy_index::ValueSpec::new(format!("v{i}"))).collect());
    }
    Segment::for_spec(&b.build().expect("a spec"))
}

fn check_books(seg: &Segment, model: &Model, rng: &mut Rng) {
    let st = seg.stats();
    assert_eq!(st.entries, model.rows.len() as u64, "entries");
    assert_eq!(st.duplicates, model.duplicates(), "duplicates");
    assert_eq!(st.coerce_failures, model.coerce_failures, "coerce failures");
    let mut seen = Vec::new();
    seg.each_entry(|k, v| seen.push((v.clone(), k.to_vec())));
    assert_eq!(seen, model.sorted(), "each_entry walks in order");
    assert_eq!(seg.max_value(), model.sorted().last().map(|(v, _)| v.clone()));
    for (k, (v, vals)) in model.rows.iter().take(8) {
        assert!(seg.contains(v, k), "contains {k:?}");
        assert_eq!(seg.stored_row(v, k), *vals, "stored_row {k:?}");
        for (f, want) in vals.iter().enumerate() {
            assert_eq!(seg.stored(v, k, f), *want, "stored {k:?} {f}");
        }
        if let Some(d) = seg.key_dir() {
            assert_eq!(d.get(k).as_ref(), Some(v), "key dir {k:?}");
        }
    }
    if let Some(d) = seg.key_dir() {
        assert_eq!(d.len(), model.rows.len(), "key dir rows");
        assert_eq!(d.get(b"row:absent"), None);
    }
    let probe = key(rng, 0);
    if !model.rows.contains_key(&probe) {
        assert!(!seg.contains(&IndexValue::I64(0), &probe));
    }
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

fn collect(mut scan: kevy_index::Scan<'_>) -> Vec<(IndexValue, Vec<u8>)> {
    let mut out = Vec::new();
    while let Some((v, k)) = scan.next_entry() {
        out.push((v.clone(), k.to_vec()));
    }
    out
}

fn check_scans(seg: &Segment, model: &Model, rng: &mut Rng) {
    let all = model.sorted();
    assert_eq!(
        collect(seg.scan(None, SortOrder::Desc)),
        all.iter().rev().cloned().collect::<Vec<_>>()
    );
    let Some((v, k)) = all.get(rng.below(all.len() as u64 + 1) as usize) else { return };
    // a cursor may name a key no longer (or never) held
    let k = if rng.below(3) == 0 { [k.as_slice(), b"~"].concat() } else { k.clone() };
    let after = Cursor::new(v.clone(), k.clone());
    let want_up: Vec<_> = all.iter().filter(|e| (&e.0, &e.1) > (v, &k)).cloned().collect();
    assert_eq!(
        collect(seg.scan(Some(&after), SortOrder::Asc)),
        want_up,
        "ascending past {after:?}"
    );
    let want_down: Vec<_> = all.iter().rev().filter(|e| (&e.0, &e.1) < (v, &k)).cloned().collect();
    assert_eq!(
        collect(seg.scan(Some(&after), SortOrder::Desc)),
        want_down,
        "descending past {after:?}"
    );
}

/// The clause engine against the model: FILTER on column 0, SORT by
/// column 1 as i64, DISTINCT and FACET on column 0.
fn check_clauses(seg: &Segment, model: &Model, lo: &IndexValue, hi: &IndexValue, arity: usize) {
    if arity < 2 {
        return;
    }
    let rows: Vec<_> = model
        .rows
        .iter()
        .filter(|(_, (v, _))| v >= lo && v <= hi)
        .map(|(k, (v, s))| (v.clone(), k.clone(), s.clone()))
        .collect::<Vec<_>>();
    let mut rows = rows;
    rows.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    let test = ValueTest::eq(ValType::Str, b"c1").expect("a str test");
    let pass = |s: &Stored| s[0].as_deref().is_some_and(|raw| test.passes(raw));
    let f = [(0, test.clone())];
    assert_eq!(seg.count_claused(lo, hi, &f), rows.iter().filter(|r| pass(&r.2)).count() as u64);
    let page = seg.query_claused(lo, hi, None, &ScalarClauses::new(3).with_filters(&f));
    let want: Vec<_> = rows.iter().filter(|r| pass(&r.2)).take(3).map(|r| r.1.clone()).collect();
    assert_eq!(page.hits.iter().map(|h| h.key.clone()).collect::<Vec<_>>(), want, "FILTER page");
    let sort = ScalarClauses::new(4).with_sort(1, SortOrder::Desc, ValType::I64);
    let page = seg.query_claused(lo, hi, None, &sort);
    let mut by_sort: Vec<_> = rows
        .iter()
        .map(|r| (r.2[1].as_deref().and_then(|b| order_key(ValType::I64, b)), r.1.clone()))
        .collect();
    by_sort.sort_by(|a, b| {
        kevy_text::sorted_order((a.0.as_deref(), &a.1), (b.0.as_deref(), &b.1), SortOrder::Desc)
    });
    let want: Vec<_> = by_sort.into_iter().take(4).map(|(_, k)| k).collect();
    assert_eq!(page.hits.iter().map(|h| h.key.clone()).collect::<Vec<_>>(), want, "SORT top 4");
    let fs = [(0, ValType::Str)];
    let page = seg.query_claused(lo, hi, None, &ScalarClauses::new(1).with_facets(&fs));
    let mut counts: HashMap<Vec<u8>, u64> = HashMap::new();
    for r in &rows {
        if let Some(b) = &r.2[0] {
            *counts.entry(b.clone()).or_default() += 1;
        }
    }
    let got: HashMap<Vec<u8>, u64> =
        page.facets[0].iter().map(|(_, l, n)| (l.clone(), *n)).collect();
    assert_eq!(got, counts, "FACET");
    let distinct =
        seg.query_claused(lo, hi, None, &ScalarClauses::new(100).with_distinct(0, ValType::Str));
    let groups = rows
        .iter()
        .filter(|r| r.2[0].is_some())
        .map(|r| r.2[0].clone())
        .collect::<std::collections::HashSet<_>>();
    let absent = rows.iter().filter(|r| r.2[0].is_none()).count();
    assert_eq!(distinct.hits.len(), (groups.len() + absent).min(100), "DISTINCT");
}

struct Run {
    seg: Segment,
    model: Model,
    rng: Rng,
    kind: Kind,
    arity: usize,
}

impl Run {
    fn write(&mut self, step: usize) {
        let k = key(&mut self.rng, step);
        let old = self.model.rows.get(&k).map(|(v, _)| v.clone());
        match self.rng.below(12) {
            0..=4 => {
                let v = value(&mut self.rng, self.kind);
                let s = stored(&mut self.rng, self.arity);
                self.seg.apply_with_values(&k, old.as_ref(), Some(v.clone()), &refs(&s));
                self.model.rows.insert(k, (v, s));
            }
            5 => {
                // the same value again, maybe with new stored values
                if let Some(v) = old {
                    let s = stored(&mut self.rng, self.arity);
                    self.seg.apply_with_values(&k, Some(&v), Some(v.clone()), &refs(&s));
                    self.model.rows.insert(k, (v, s));
                }
            }
            6 => {
                self.seg.apply(&k, old.as_ref(), None);
                self.model.rows.remove(&k);
                self.model.coerce_failures += 1;
            }
            7 => {
                if let Some(v) = old {
                    self.seg.remove(&k, &v);
                    self.model.rows.remove(&k);
                }
            }
            8 => {
                let bound = value(&mut self.rng, self.kind);
                let preview = collect(self.seg.scan_below(&bound));
                let got = self.seg.split_off_below(&bound);
                let want: Vec<_> =
                    self.model.sorted().into_iter().filter(|(v, _)| *v < bound).collect();
                self.model.rows.retain(|_, (v, _)| *v >= bound);
                assert_eq!(preview, want, "scan_below {bound:?}");
                assert_eq!(got, want, "split_off_below {bound:?}");
            }
            9 => self.seg.repack(),
            10 => self.seg.set_key_dir(self.rng.below(2) == 0),
            _ => {
                let (a, b) = (value(&mut self.rng, self.kind), value(&mut self.rng, self.kind));
                let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
                check_range(&self.seg, &self.model, &lo, &hi, 1 + self.rng.below(4) as usize);
                check_scans(&self.seg, &self.model, &mut self.rng);
                check_clauses(&self.seg, &self.model, &lo, &hi, self.arity);
            }
        }
    }
}

fn run(seed: u64, kind: Kind, arity: usize, shaped: bool) {
    let mut r = Run {
        seg: segment(kind, arity, shaped),
        model: Model::default(),
        rng: Rng(seed),
        kind,
        arity,
    };
    for step in 0..1500 {
        r.write(step);
        check_books(&r.seg, &r.model, &mut r.rng);
    }
}

#[test]
fn i64_segments_follow_the_model() {
    for seed in 1..=8 {
        run(seed, Kind::I64, (seed % 3) as usize, seed % 2 == 0);
    }
}

#[test]
fn f64_segments_follow_the_model() {
    for seed in 51..=56 {
        run(seed, Kind::F64, (seed % 3) as usize, seed % 2 == 0);
    }
}

#[test]
fn str_segments_follow_the_model() {
    for seed in 101..=108 {
        run(seed, Kind::Str, (seed % 3) as usize, seed % 2 == 0);
    }
}

#[test]
fn composite_segments_follow_the_model() {
    for seed in 201..=206 {
        // a composite spec declares no VALUES; the unshaped segment stores two
        let shaped = seed % 2 == 0;
        run(seed, Kind::Composite, if shaped { 0 } else { 2 }, shaped);
    }
}

/// Enough rows for a tree several levels deep, with the same checks at
/// the end.
#[test]
fn a_large_segment_follows_the_model() {
    let mut r = Run {
        seg: segment(Kind::I64, 2, true),
        model: Model::default(),
        rng: Rng(9),
        kind: Kind::I64,
        arity: 2,
    };
    for i in 0..60_000u64 {
        let k = format!("row:{}", i * 7919 % 60_000).into_bytes();
        let v = IndexValue::I64((i % 997) as i64);
        let s = stored(&mut r.rng, 2);
        let old = r.model.rows.get(&k).map(|(v, _)| v.clone());
        r.seg.apply_with_values(&k, old.as_ref(), Some(v.clone()), &refs(&s));
        r.model.rows.insert(k, (v, s));
    }
    for step in 0..3000 {
        r.write(step);
    }
    check_books(&r.seg, &r.model, &mut r.rng);
    check_range(&r.seg, &r.model, &IndexValue::I64(100), &IndexValue::I64(300), 500);
    check_clauses(&r.seg, &r.model, &IndexValue::I64(0), &IndexValue::I64(996), 2);
}
