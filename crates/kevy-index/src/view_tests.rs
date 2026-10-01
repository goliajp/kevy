//! Tests for [`crate::view`] + the sidecar round-trip (child module of
//! `view` via `#[path]`, so private items stay reachable).

use super::*;
use crate::segment::Segment;

fn seg_ab() -> (Segment, Segment) {
    let mut a = Segment::new();
    let mut b = Segment::new();
    for i in 0..10 {
        a.apply(format!("k{i}").as_bytes(), None, Some(IndexValue::I64(i)));
        if i % 2 == 0 {
            b.apply(format!("k{i}").as_bytes(), None, Some(IndexValue::Str(b"eng".to_vec())));
        }
    }
    // per-key membership reads each index's key directory
    a.set_key_dir(true);
    b.set_key_dir(true);
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
    let mut got = and.eval(&seg);
    got.sort();
    assert_eq!(got, vec![b"k2".to_vec(), b"k4".to_vec(), b"k6".to_vec()]);

    let or = Tree::Or(Box::new(age.clone()), Box::new(eng.clone()));
    assert_eq!(or.eval(&seg).len(), 8, "2..=7 ∪ evens = 8");

    let diff = Tree::Diff(Box::new(age.clone()), Box::new(eng.clone()));
    let mut got = diff.eval(&seg);
    got.sort();
    assert_eq!(got, vec![b"k3".to_vec(), b"k5".to_vec(), b"k7".to_vec()]);

    // per-key membership mirrors set eval
    assert!(and.contains(b"k4", &seg));
    assert!(!and.contains(b"k3", &seg));
    assert!(diff.contains(b"k5", &seg));
    assert!(!diff.contains(b"k4", &seg));
    assert!(or.contains(b"k3", &seg), "in the age range only");
    assert!(or.contains(b"k8", &seg), "in the dept only");
    assert!(!or.contains(b"k9", &seg));
}

#[test]
fn an_index_without_a_key_directory_holds_no_key() {
    let mut plain = Segment::new();
    plain.apply(b"k1", None, Some(IndexValue::I64(1)));
    let seg = |_: &[u8]| -> Option<&Segment> { Some(&plain) };
    assert!(!leaf("age", IndexValue::I64(0), IndexValue::I64(9)).contains(b"k1", &seg));
}

#[test]
fn a_view_through_a_template_keeps_it() {
    let spec = ViewSpec::new("v", leaf("age", IndexValue::I64(0), IndexValue::I64(1)), "age")
        .with_via(b"user:{key.1}".to_vec());
    assert_eq!(spec.via.as_deref(), Some(&b"user:{key.1}"[..]));
}

#[test]
fn an_ascending_bound_evicts_its_largest_as_smaller_ones_arrive() {
    let mut m = MaterializedSet::new(4, kevy_text::SortOrder::Asc); // cap 5
    assert!(m.is_empty());
    for i in (0..8).rev() {
        m.apply(format!("k{i}").as_bytes(), Membership::Member(Some(IndexValue::I64(i))));
    }
    assert!(!m.is_empty());
    let keys: Vec<Vec<u8>> = m.page(None, 10).into_iter().map(|(_, k)| k).collect();
    assert_eq!(keys, [b"k0", b"k1", b"k2", b"k3", b"k4"].map(|k| k.to_vec()));
}

#[test]
fn a_page_resumes_just_past_its_cursor_in_either_order() {
    for order in [kevy_text::SortOrder::Asc, kevy_text::SortOrder::Desc] {
        let mut m = MaterializedSet::new(0, order);
        for i in 0..6 {
            m.apply(format!("k{i}").as_bytes(), Membership::Member(Some(IndexValue::I64(i))));
        }
        let first = m.page(None, 2);
        let rest = m.page(first.last(), 10);
        let all: Vec<i64> = first
            .iter()
            .chain(&rest)
            .map(|(v, _)| match v {
                IndexValue::I64(n) => *n,
                other => panic!("{other:?}"),
            })
            .collect();
        let want: Vec<i64> = if order == kevy_text::SortOrder::Asc {
            (0..6).collect()
        } else {
            (0..6).rev().collect()
        };
        assert_eq!(all, want, "{order:?}");
    }
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
        order: kevy_text::SortOrder::Asc,
        mode: ViewMode::Virtual,
        via: None,
    };
    assert!(spec.validate().is_err(), "depth 4 rejected");
}

#[test]
fn materialized_bounds_and_underflow() {
    let mut m = MaterializedSet::new(4, kevy_text::SortOrder::Asc); // cap = 4 + 1 = 5
    for i in 0..8 {
        let under =
            m.apply(format!("k{i}").as_bytes(), Membership::Member(Some(IndexValue::I64(i))));
        assert!(!under);
    }
    assert_eq!(m.len(), 5, "bounded at K+Δ");
    let page = m.page(None, 10);
    assert_eq!(page[0].1, b"k0".to_vec(), "best kept");
    assert_eq!(page.last().unwrap().1, b"k4".to_vec(), "worst evicted");
}

#[test]
fn materialized_desc_bound_keeps_largest() {
    let mut m = MaterializedSet::new(4, kevy_text::SortOrder::Desc); // cap 5
    for i in 0..8 {
        m.apply(format!("k{i}").as_bytes(), Membership::Member(Some(IndexValue::I64(i))));
    }
    assert_eq!(m.len(), 5);
    let page = m.page(None, 10);
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
        order: kevy_text::SortOrder::Desc,
        mode: ViewMode::Materialized { top_k: 50 },
        via: Some(b"user:{key.1}".to_vec()),
    };
    let mut c = ViewCatalog::new();
    c.create(spec.clone()).unwrap();
    c.create(ViewSpec {
        name: b"v2".to_vec(),
        tree: leaf("age", IndexValue::I64(0), IndexValue::I64(1)),
        order_by: b"age".to_vec(),
        order: kevy_text::SortOrder::Asc,
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
    let mut m = MaterializedSet::new(4, kevy_text::SortOrder::Asc);
    for i in 0..5 {
        m.apply(format!("k{i}").as_bytes(), Membership::Member(Some(IndexValue::I64(i))));
    }
    assert_eq!(m.len(), 5);
    assert!(!m.apply(b"k0", Membership::NonMember), "5→4 = still K");
    assert!(m.apply(b"k1", Membership::NonMember), "4→3 < K → underflow signal");
    // order-index-excluded members are counted, not stored
    m.apply(b"kx", Membership::Member(None));
    assert_eq!(m.order_excluded(), 1);
    // unbounded never underflows
    let mut u = MaterializedSet::new(0, kevy_text::SortOrder::Asc);
    u.apply(b"a", Membership::Member(Some(IndexValue::I64(1))));
    assert!(!u.apply(b"a", Membership::NonMember));
}
