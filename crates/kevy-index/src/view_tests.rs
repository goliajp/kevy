//! Tests for [`crate::view`] + the sidecar round-trip (child module of
//! `view` via `#[path]`, so private items stay reachable).

use super::*;
use crate::segment::Segment;

fn seg_ab() -> (Segment, Segment) {
    let mut a = Segment::new();
    let mut b = Segment::new();
    for i in 0..10 {
        a.apply(format!("k{i}").as_bytes(), Some(IndexValue::I64(i)));
        if i % 2 == 0 {
            b.apply(format!("k{i}").as_bytes(), Some(IndexValue::Str(b"eng".to_vec())));
        }
    }
    (a, b)
}

fn leaf(idx: &str, min: IndexValue, max: IndexValue) -> Tree {
    Tree::Leaf(Leaf { index: idx.into(), min, max })
}

#[test]
fn tree_eval_and_or_diff() {
    let (a, b) = seg_ab();
    let seg = |n: &[u8]| -> Option<&Segment> {
        match n {
            b"age" => Some(&a),
            b"dept" => Some(&b),
            _ => None,
        }
    };
    let age = leaf("age", IndexValue::I64(2), IndexValue::I64(7));
    let eng = leaf("dept", IndexValue::Str(b"eng".to_vec()), IndexValue::Str(b"eng".to_vec()));
    let and = Tree::And(Box::new(age.clone()), Box::new(eng.clone()));
    let mut got = eval_tree(&and, &seg);
    got.sort();
    assert_eq!(got, vec![b"k2".to_vec(), b"k4".to_vec(), b"k6".to_vec()]);

    let or = Tree::Or(Box::new(age.clone()), Box::new(eng.clone()));
    assert_eq!(eval_tree(&or, &seg).len(), 8, "2..=7 ∪ evens = 8");

    let diff = Tree::Diff(Box::new(age.clone()), Box::new(eng.clone()));
    let mut got = eval_tree(&diff, &seg);
    got.sort();
    assert_eq!(got, vec![b"k3".to_vec(), b"k5".to_vec(), b"k7".to_vec()]);

    // per-key membership mirrors set eval
    assert!(key_in_tree(&and, b"k4", &seg));
    assert!(!key_in_tree(&and, b"k3", &seg));
    assert!(key_in_tree(&diff, b"k5", &seg));
    assert!(!key_in_tree(&diff, b"k4", &seg));
}

#[test]
fn caps_validate() {
    let l = leaf("a", IndexValue::I64(0), IndexValue::I64(1));
    let deep = Tree::And(
        Box::new(Tree::And(
            Box::new(Tree::And(Box::new(l.clone()), Box::new(l.clone()))),
            Box::new(l.clone()),
        )),
        Box::new(l.clone()),
    );
    let spec = ViewSpec {
        name: b"v".to_vec(),
        tree: deep,
        order_by: b"a".to_vec(),
        desc: false,
        mode: ViewMode::Virtual,
        via: None,
    };
    assert!(spec.validate().is_err(), "depth 4 rejected");
}

#[test]
fn materialized_bounds_and_underflow() {
    let mut m = MaterializedSet::new(4, false); // cap = 4 + 1 = 5
    for i in 0..8 {
        let under = m.apply(format!("k{i}").as_bytes(), true, Some(IndexValue::I64(i)));
        assert!(!under);
    }
    assert_eq!(m.len(), 5, "bounded at K+Δ");
    let page = m.page(None, 10, false);
    assert_eq!(page[0].1, b"k0".to_vec(), "best kept");
    assert_eq!(page.last().unwrap().1, b"k4".to_vec(), "worst evicted");
}

#[test]
fn materialized_desc_bound_keeps_largest() {
    let mut m = MaterializedSet::new(4, true); // cap 5
    for i in 0..8 {
        m.apply(format!("k{i}").as_bytes(), true, Some(IndexValue::I64(i)));
    }
    assert_eq!(m.len(), 5);
    let page = m.page(None, 10, true);
    assert_eq!(page[0].1, b"k7".to_vec(), "largest kept on top");
    assert_eq!(page.last().unwrap().1, b"k3".to_vec(), "smallest evicted");
}

#[test]
fn view_catalog_sidecar_roundtrip() {
    let tree = Tree::Diff(
        Box::new(Tree::And(
            Box::new(leaf("age", IndexValue::I64(1), IndexValue::I64(9))),
            Box::new(leaf(
                "dept",
                IndexValue::Str(b"e )n%g".to_vec()),
                IndexValue::Str(b"e )n%g".to_vec()),
            )),
        )),
        Box::new(leaf("flag", IndexValue::F64(-0.5), IndexValue::F64(2.5))),
    );
    let spec = ViewSpec {
        name: b"v one".to_vec(),
        tree,
        order_by: b"age".to_vec(),
        desc: true,
        mode: ViewMode::Materialized { top_k: 50 },
        via: Some(b"user:{key.1}".to_vec()),
    };
    let mut c = ViewCatalog::new();
    c.create(spec.clone()).unwrap();
    c.create(ViewSpec {
        name: b"v2".to_vec(),
        tree: leaf("age", IndexValue::I64(0), IndexValue::I64(1)),
        order_by: b"age".to_vec(),
        desc: false,
        mode: ViewMode::Virtual,
        via: None,
    })
    .unwrap();
    let text = c.to_sidecar();
    let c2 = ViewCatalog::from_sidecar(&text).expect("parse");
    assert_eq!(c2.len(), 2);
    assert_eq!(c2.get(b"v one").unwrap(), &spec);
    assert!(ViewCatalog::from_sidecar("junk").is_none());
}

#[test]
fn materialized_underflow_signals() {
    let mut m = MaterializedSet::new(4, false);
    for i in 0..5 {
        m.apply(format!("k{i}").as_bytes(), true, Some(IndexValue::I64(i)));
    }
    assert_eq!(m.len(), 5);
    assert!(!m.apply(b"k0", false, None), "5→4 = still K");
    assert!(m.apply(b"k1", false, None), "4→3 < K → underflow signal");
    // order-index-excluded members are counted, not stored
    m.apply(b"kx", true, None);
    assert_eq!(m.order_excluded, 1);
    // unbounded never underflows
    let mut u = MaterializedSet::new(0, false);
    u.apply(b"a", true, Some(IndexValue::I64(1)));
    assert!(!u.apply(b"a", false, None));
}
