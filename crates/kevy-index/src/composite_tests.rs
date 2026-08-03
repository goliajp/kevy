//! Composite encoding tests: order preservation across mixed types +
//! DESC, prefix-bounds exactness (brute force vs byte range), the
//! WHERE grammar, and the CREATE-time guards.

use super::*;
use crate::catalog::{Catalog, FieldSpec, IndexKind, IndexSpec};

fn col(name: &str, ty: ValType, desc: bool) -> CompositeCol {
    CompositeCol { name: name.into(), ty, desc }
}

/// Deterministic xorshift64* — no crates, stable across runs.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

/// One random raw value of `ty` (as wire bytes).
fn raw_value(rng: &mut Rng, ty: ValType) -> Vec<u8> {
    match ty {
        ValType::I64 => {
            let pool: [i64; 9] =
                [i64::MIN, -1_000_000, -7, -1, 0, 1, 7, 1_000_000, i64::MAX];
            pool[(rng.next() % 9) as usize].to_string().into_bytes()
        }
        ValType::F64 => {
            let pool: [f64; 8] = [-1e300, -2.5, -0.0, 0.0, 0.5, 2.5, 1e300, 42.0];
            format!("{}", pool[(rng.next() % 8) as usize]).into_bytes()
        }
        ValType::Str => {
            // Deliberately adversarial: empty, embedded NULs, 0xFF
            // runs, shared prefixes.
            let pool: [&[u8]; 8] = [
                b"",
                b"\x00",
                b"\x00\x00a",
                b"\xFF\xFF",
                b"a",
                b"ab",
                b"a\x00b",
                b"b",
            ];
            pool[(rng.next() % 8) as usize].to_vec()
        }
        ValType::Vector => unreachable!(),
    }
}

/// Compare two tuples column-wise the way the composite MUST order
/// them (the reference model the byte encoding is checked against).
fn model_cmp(cols: &[CompositeCol], a: &[Vec<u8>], b: &[Vec<u8>]) -> std::cmp::Ordering {
    for (i, c) in cols.iter().enumerate() {
        let (va, vb) = (
            IndexValue::coerce(c.ty, &a[i]).unwrap(),
            IndexValue::coerce(c.ty, &b[i]).unwrap(),
        );
        let ord = if c.desc { vb.cmp(&va) } else { va.cmp(&vb) };
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    std::cmp::Ordering::Equal
}

fn schema() -> Vec<CompositeCol> {
    vec![
        col("a", ValType::Str, false),
        col("b", ValType::I64, true),
        col("c", ValType::F64, false),
        col("d", ValType::Str, true),
    ]
}

fn random_tuples(n: usize) -> (Vec<CompositeCol>, Vec<Vec<Vec<u8>>>) {
    let cols = schema();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let tuples: Vec<Vec<Vec<u8>>> = (0..n)
        .map(|_| cols.iter().map(|c| raw_value(&mut rng, c.ty)).collect())
        .collect();
    (cols, tuples)
}

fn encode_tuple(cols: &[CompositeCol], t: &[Vec<u8>]) -> Vec<u8> {
    let refs: Vec<Option<&[u8]>> = t.iter().map(|v| Some(v.as_slice())).collect();
    composite_encode(cols, &refs).expect("encodes")
}

/// The property: memcmp of encodings == the column-wise tuple order,
/// on every pair of 300 adversarial random tuples (mixed types, DESC
/// components, NULs, 0xFF runs, shared prefixes).
#[test]
fn encoding_order_matches_tuple_order_property() {
    let (cols, tuples) = random_tuples(300);
    let encoded: Vec<Vec<u8>> = tuples.iter().map(|t| encode_tuple(&cols, t)).collect();
    for i in 0..tuples.len() {
        for j in (i + 1)..tuples.len() {
            assert_eq!(
                encoded[i].cmp(&encoded[j]),
                model_cmp(&cols, &tuples[i], &tuples[j]),
                "pair {i}/{j}: {:?} vs {:?}",
                tuples[i],
                tuples[j],
            );
        }
    }
}

/// Bounds exactness: for equality prefixes of every depth (with and
/// without a range on the next component), the byte range selects
/// EXACTLY the tuples the predicate selects — brute-forced over the
/// same random population.
#[test]
fn prefix_bounds_select_exactly_the_predicate_set() {
    let (cols, tuples) = random_tuples(300);
    let encoded: Vec<Vec<u8>> = tuples.iter().map(|t| encode_tuple(&cols, t)).collect();
    let probe = &tuples[17];
    for depth in 1..=cols.len() {
        let w = WhereClause {
            eqs: (0..depth).map(|i| (cols[i].name.clone(), probe[i].clone())).collect(),
            range: None,
        };
        let (lo, hi) = composite_bounds(&cols, &w, 0).expect("bounds");
        for (t, e) in tuples.iter().zip(&encoded) {
            let want = (0..depth).all(|i| {
                IndexValue::coerce(cols[i].ty, &t[i]) == IndexValue::coerce(cols[i].ty, &probe[i])
            });
            let got = *e >= lo && *e <= hi;
            assert_eq!(got, want, "depth {depth}: tuple {t:?}");
        }
    }
}

/// Equality prefix + range on the next component — including a DESC
/// ranged component (endpoint swap) and an unconstrained tail.
#[test]
fn equality_prefix_plus_range_bounds() {
    let (cols, tuples) = random_tuples(300);
    let encoded: Vec<Vec<u8>> = tuples.iter().map(|t| encode_tuple(&cols, t)).collect();
    let probe = &tuples[3];
    // eq on a, range on b (i64 DESC) — tail c, d unconstrained.
    let w = WhereClause {
        eqs: vec![(b"a".to_vec(), probe[0].clone())],
        range: Some((b"b".to_vec(), b"-7".to_vec(), b"1000000".to_vec())),
    };
    let (lo, hi) = composite_bounds(&cols, &w, 0).expect("bounds");
    for (t, e) in tuples.iter().zip(&encoded) {
        let same_a =
            IndexValue::coerce(ValType::Str, &t[0]) == IndexValue::coerce(ValType::Str, &probe[0]);
        let b_val = match IndexValue::coerce(ValType::I64, &t[1]).unwrap() {
            IndexValue::I64(v) => v,
            _ => unreachable!(),
        };
        let want = same_a && (-7..=1_000_000).contains(&b_val);
        assert_eq!(*e >= lo && *e <= hi, want, "tuple {t:?}");
    }
}

/// A row missing a component column has no encoding — excluded, and an
/// over-long str component likewise.
#[test]
fn missing_or_overlong_component_excludes_the_row() {
    let cols = schema();
    let missing: Vec<Option<&[u8]>> = vec![Some(b"x"), None, Some(b"1.0"), Some(b"y")];
    assert!(composite_encode(&cols, &missing).is_none());
    let long = vec![b'z'; MAX_STR_COMPONENT + 1];
    let over: Vec<Option<&[u8]>> =
        vec![Some(&long), Some(b"1"), Some(b"1.0"), Some(b"y")];
    assert!(composite_encode(&cols, &over).is_none());
    let coerce_fail: Vec<Option<&[u8]>> =
        vec![Some(b"x"), Some(b"not-a-number"), Some(b"1.0"), Some(b"y")];
    assert!(composite_encode(&cols, &coerce_fail).is_none());
    let at_cap = vec![b'z'; MAX_STR_COMPONENT];
    let ok: Vec<Option<&[u8]>> = vec![Some(&at_cap), Some(b"1"), Some(b"1.0"), Some(b"y")];
    assert!(composite_encode(&cols, &ok).is_some(), "AT the cap still encodes");
}

#[test]
fn where_grammar_parses_and_refuses() {
    let a = |s: &str| s.as_bytes().to_vec();
    let stop = |t: &[u8]| t.eq_ignore_ascii_case(b"LIMIT");
    // eq + eq + range, then a tail keyword.
    let argv = vec![
        a("x"), a("EQ"), a("1"), a("y"), a("EQ"), a("2"),
        a("RANGE"), a("z"), a("0"), a("9"), a("LIMIT"), a("5"),
    ];
    let (w, next) = parse_where(&argv, 0, stop).expect("parses");
    assert_eq!(w.eqs.len(), 2);
    assert_eq!(w.range, Some((a("z"), a("0"), a("9"))));
    assert_eq!(next, 10);
    // empty WHERE refused.
    assert!(parse_where(&[a("LIMIT"), a("5")], 0, stop).is_none());
    // col without EQ refused.
    assert!(parse_where(&[a("x"), a("NEQ"), a("1")], 0, stop).is_none());
    // truncated RANGE refused.
    assert!(parse_where(&[a("RANGE"), a("z"), a("0")], 0, stop).is_none());
}

#[test]
fn bounds_errors_are_named() {
    let cols = schema();
    let unknown = WhereClause { eqs: vec![(b"nope".to_vec(), b"1".to_vec())], range: None };
    let e = composite_bounds(&cols, &unknown, 0).unwrap_err();
    assert!(e.contains("'nope'") && e.contains("does not declare"), "{e}");
    // declared but out of order (b before a) — the prefix rule.
    let out_of_order = WhereClause { eqs: vec![(b"b".to_vec(), b"1".to_vec())], range: None };
    let e = composite_bounds(&cols, &out_of_order, 0).unwrap_err();
    assert!(e.contains("leading prefix"), "{e}");
    // a bound that does not coerce.
    let bad = WhereClause {
        eqs: vec![(b"a".to_vec(), b"x".to_vec())],
        range: Some((b"b".to_vec(), b"cheap".to_vec(), b"9".to_vec())),
    };
    let e = composite_bounds(&cols, &bad, 0).unwrap_err();
    assert!(e.contains("not a valid i64"), "{e}");
}

fn composite_spec(name: &str) -> IndexSpec {
    let mut s = IndexSpec::single_field(
        name.into(),
        b"t:".to_vec(),
        b"a".to_vec(),
        ValType::Str,
        IndexKind::Range,
    );
    s.composite = Some(vec![col("a", ValType::Str, false), col("b", ValType::I64, true)]);
    s
}

#[test]
fn create_guards_refuse_bad_composite_combos_by_name() {
    let mut c = Catalog::new();
    c.create(composite_spec("ok")).expect("a legal composite creates");

    let mut wrong_kind = composite_spec("k");
    wrong_kind.kind = IndexKind::Unique;
    assert_eq!(Catalog::new().create(wrong_kind), Err("ERR COMPOSITE requires KIND range"));

    let mut wrong_ty = composite_spec("t");
    wrong_ty.ty = ValType::I64;
    assert_eq!(Catalog::new().create(wrong_ty), Err("ERR COMPOSITE requires TYPE str"));

    let mut with_values = composite_spec("v");
    with_values.values = vec![crate::ValueSpec::new(b"c".to_vec())];
    assert_eq!(
        Catalog::new().create(with_values),
        Err("ERR COMPOSITE cannot combine with VALUES")
    );

    let mut multi_fields = composite_spec("f");
    multi_fields.fields =
        vec![FieldSpec::new(b"a".to_vec()), FieldSpec::new(b"b".to_vec())];
    // The generic non-text multi-field fence fires first — still a
    // named refusal, never accept-and-ignore.
    assert!(Catalog::new().create(multi_fields).is_err());

    let mut empty = composite_spec("e");
    empty.composite = Some(Vec::new());
    assert_eq!(Catalog::new().create(empty), Err("ERR COMPOSITE needs at least one column"));

    let mut too_many = composite_spec("m");
    too_many.composite =
        Some((0..9).map(|i| col(&format!("c{i}"), ValType::I64, false)).collect());
    assert_eq!(Catalog::new().create(too_many), Err("ERR COMPOSITE supports at most 8 columns"));

    let mut vec_col = composite_spec("vv");
    vec_col.composite = Some(vec![col("a", ValType::Vector, false)]);
    assert_eq!(Catalog::new().create(vec_col), Err("ERR COMPOSITE columns must be i64|f64|str"));
}

/// The spec-level derivation face: composite read names, primary
/// width, and derive_scalar (exclusion included).
#[test]
fn spec_derivation_reads_composite_columns() {
    let spec = composite_spec("op");
    assert_eq!(spec.scalar_read_names(), vec![b"a".as_slice(), b"b".as_slice()]);
    assert_eq!(spec.primary_width(), 2);
    let v = spec
        .derive_scalar(&[Some(b"x".to_vec()), Some(b"5".to_vec())])
        .expect("derives");
    let cols = spec.composite.as_ref().unwrap();
    let expect =
        composite_encode(cols, &[Some(b"x"), Some(b"5")]).expect("encodes");
    assert_eq!(v, IndexValue::Str(expect));
    assert!(spec.derive_scalar(&[Some(b"x".to_vec()), None]).is_none(), "missing col excludes");

    // The plain single-field face is unchanged.
    let plain = IndexSpec::single_field(
        b"p".to_vec(), b"t:".to_vec(), b"n".to_vec(), ValType::I64, IndexKind::Range,
    );
    assert_eq!(plain.scalar_read_names(), vec![b"n".as_slice()]);
    assert_eq!(plain.primary_width(), 1);
    assert_eq!(plain.derive_scalar(&[Some(b"7".to_vec())]), Some(IndexValue::I64(7)));
}
