//! SegList (element-granularity COW) tests: promotion, semantics vs a
//! model, per-segment COW behavior under a pinned snapshot view, and
//! accounting round-trips.

use crate::Store;
use crate::list_seg::{SEG_CAP, SEG_PROMOTE};
use crate::value::Value;
use alloc::sync::Arc;

fn rpush_n(st: &mut Store, key: &[u8], n: usize, tag: u8) {
    // Values > inline-list caps so the heap encodings engage; distinct
    // per index so order checks are meaningful.
    for i in 0..n {
        let v = alloc::format!("elem-{tag}-{i:010}");
        st.rpush(key, &[v.as_bytes()]).unwrap();
    }
}

fn is_seglist(st: &Store, key: &[u8]) -> bool {
    matches!(st.map.get(key).map(|e| &e.value), Some(Value::SegList(_)))
}

#[test]
fn push_promotes_at_threshold_and_preserves_order() {
    let mut st = Store::new();
    rpush_n(&mut st, b"l", SEG_PROMOTE + 5, 0);
    assert!(is_seglist(&st, b"l"), "list past SEG_PROMOTE must be segmented");
    assert_eq!(st.llen(b"l").unwrap(), SEG_PROMOTE + 5);
    // Order intact across the promotion + segment boundary.
    assert_eq!(
        st.lindex(b"l", 0).unwrap().unwrap(),
        alloc::format!("elem-0-{:010}", 0).into_bytes()
    );
    assert_eq!(
        st.lindex(b"l", (SEG_PROMOTE + 4) as i64).unwrap().unwrap(),
        alloc::format!("elem-0-{:010}", SEG_PROMOTE + 4).into_bytes()
    );
    assert_eq!(
        st.lindex(b"l", SEG_CAP as i64).unwrap().unwrap(),
        alloc::format!("elem-0-{:010}", SEG_CAP).into_bytes()
    );
    // An LRANGE spanning the segment boundary.
    let span = st.lrange(b"l", SEG_CAP as i64 - 2, SEG_CAP as i64 + 1).unwrap();
    assert_eq!(span.len(), 4);
    assert_eq!(span[2], alloc::format!("elem-0-{:010}", SEG_CAP).into_bytes());
}

#[test]
fn load_list_applies_the_encoding_switch() {
    let mut st = Store::new();
    let big: alloc::vec::Vec<alloc::vec::Vec<u8>> =
        (0..SEG_PROMOTE + 1).map(|i| alloc::format!("x{i}").into_bytes()).collect();
    st.load_list(b"big".to_vec(), big, None);
    assert!(is_seglist(&st, b"big"));
    assert_eq!(st.llen(b"big").unwrap(), SEG_PROMOTE + 1);

    let small: alloc::vec::Vec<alloc::vec::Vec<u8>> =
        (0..100).map(|i| alloc::format!("y{i}").into_bytes()).collect();
    st.load_list(b"small".to_vec(), small, None);
    assert!(!is_seglist(&st, b"small"));
    assert_eq!(st.llen(b"small").unwrap(), 100);
}

/// The arc's headline claim: a write to a view-pinned segmented list
/// clones the touched segment, not the value. Untouched segments stay
/// shared with the view.
#[test]
fn cow_write_under_pinned_view_clones_only_touched_segment() {
    let mut st = Store::new();
    rpush_n(&mut st, b"l", 3 * SEG_CAP + 10, 0);
    assert!(is_seglist(&st, b"l"));

    let view = st.collect_snapshot();
    st.rpush(b"l", &[b"after-pin".as_slice()]).unwrap();

    let Some(Value::SegList(live)) = st.map.get(b"l".as_slice()).map(|e| &e.value) else {
        panic!("still segmented");
    };
    let shared = live.seg_arcs().filter(|s| Arc::strong_count(s) >= 2).count();
    let unique = live.seg_arcs().filter(|s| Arc::strong_count(s) == 1).count();
    // Only the tail segment (the one the push touched) was cloned.
    assert_eq!(unique, 1, "exactly the touched segment is unshared");
    assert!(shared >= 3, "untouched segments stay view-shared");

    // The view still serializes the pre-write state.
    let mut view_len = 0usize;
    view.each(|_, v, _| {
        if let Value::SegList(l) = v {
            view_len = l.len();
        }
    });
    assert_eq!(view_len, 3 * SEG_CAP + 10);
    assert_eq!(st.llen(b"l").unwrap(), 3 * SEG_CAP + 11);
}

/// Middle-of-list ops on the segmented encoding agree with a model.
#[test]
fn segged_ops_match_model_semantics() {
    let mut st = Store::new();
    let n = SEG_PROMOTE + 100;
    rpush_n(&mut st, b"l", n, 1);
    let mut model: alloc::vec::Vec<alloc::vec::Vec<u8>> =
        (0..n).map(|i| alloc::format!("elem-1-{i:010}").into_bytes()).collect();

    // LSET deep in the second segment.
    let at = SEG_CAP + 50;
    st.lset(b"l", at as i64, b"replaced").unwrap();
    model[at] = b"replaced".to_vec();

    // LINSERT before a pivot in the first segment.
    let pivot = alloc::format!("elem-1-{:010}", 123);
    let r = st.linsert(b"l", true, pivot.as_bytes(), b"inserted").unwrap();
    assert_eq!(r, (n + 1) as i64);
    model.insert(123, b"inserted".to_vec());

    // LREM tail-first of a repeated value.
    st.rpush(b"l", &[b"dup".as_slice(), b"dup".as_slice(), b"dup".as_slice()]).unwrap();
    model.extend([b"dup".to_vec(), b"dup".to_vec(), b"dup".to_vec()]);
    let removed = st.lrem(b"l", -2, b"dup").unwrap();
    assert_eq!(removed, 2);
    model.truncate(model.len() - 2);

    // LTRIM to a window crossing a segment boundary.
    let (s, e) = (SEG_CAP - 10, SEG_CAP + 60);
    st.ltrim(b"l", s as i64, e as i64).unwrap();
    let model: alloc::vec::Vec<_> = model[s..=e].to_vec();

    assert_eq!(st.llen(b"l").unwrap(), model.len());
    let got = st.lrange(b"l", 0, -1).unwrap();
    assert_eq!(got, model);

    // Pops from both ends drain in model order.
    let front = st.lpop(b"l", 3).unwrap();
    assert_eq!(front, model[..3].to_vec());
    let back = st.rpop(b"l", 3).unwrap();
    let mut expect_back = model[model.len() - 3..].to_vec();
    expect_back.reverse();
    assert_eq!(back, expect_back);
}

/// Weight accounting survives the whole segged-op zoo: after deleting
/// the key, `used_memory` returns to the pre-key baseline.
#[test]
fn accounting_round_trips_through_segged_ops() {
    let mut st = Store::new();
    let baseline = st.used_memory();
    rpush_n(&mut st, b"l", SEG_PROMOTE + 200, 2);
    st.lset(b"l", 17, b"x").unwrap();
    st.linsert(b"l", false, b"x", b"y").unwrap();
    st.lrem(b"l", 0, b"y").unwrap();
    st.ltrim(b"l", 100, (SEG_PROMOTE - 50) as i64).unwrap();
    st.lpop(b"l", 25).unwrap();
    st.rpop(b"l", 25).unwrap();
    assert!(st.used_memory() > baseline);
    assert_eq!(st.del(&[b"l".as_slice()]), 1);
    assert_eq!(st.used_memory(), baseline);
}

/// LTRIM to an empty range deletes the key (any encoding).
#[test]
fn trim_to_nothing_deletes_key() {
    let mut st = Store::new();
    rpush_n(&mut st, b"l", SEG_PROMOTE + 10, 3);
    assert!(is_seglist(&st, b"l"));
    st.ltrim(b"l", 5, 1).unwrap(); // inverted range = clear
    assert_eq!(st.llen(b"l").unwrap(), 0);
    assert!(st.map.get(b"l".as_slice()).is_none(), "emptied key is removed");
}
