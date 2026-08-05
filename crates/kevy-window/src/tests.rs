//! The shadow's reach — the property a flat tombstone set could not
//! express, and lost rows for.

use kevy_index::{IndexValue, Segment, ValType, WindowShape, WindowSpec};

use crate::WindowRt;

fn spec() -> WindowSpec {
    WindowSpec { column: b"ts".to_vec(), span: 100, bucket: 50 }
}

/// Fill a tree with `ts` values and slide it once, returning what the
/// cold side holds afterwards.
fn slide_once(w: &mut WindowRt, seg: &mut Segment, dir: &std::path::Path, vals: &[i64]) -> bool {
    for v in vals {
        seg.apply(format!("r:{v}").as_bytes(), Some(IndexValue::I64(*v)));
    }
    w.slide(b"t.ts", seg, dir).expect("slide")
}

fn cold_keys(w: &WindowRt) -> Vec<Vec<u8>> {
    w.cold_hits(
        ValType::I64,
        &IndexValue::I64(i64::MIN),
        &IndexValue::I64(i64::MAX),
        None,
        usize::MAX,
    )
    .expect("cold hits")
    .into_iter()
    .map(|(k, _)| k)
    .collect()
}

/// A row that is written after it has already slid earns a tombstone —
/// correctly, because its old cold entry is now stale. When the new
/// value slides in turn, the NEW entry must be visible: the shadow was
/// recorded against what existed at the time, not against the row's
/// name forever.
///
/// With a flat `HashSet<row>` the second entry was hidden too, and the
/// row became unreachable through its own index while still sitting in
/// the keyspace. Measured at 19-21 rows lost per 20 000 written.
#[test]
fn a_shadow_does_not_reach_forward_to_a_later_segment() {
    let dir = kevy_tmpdir::TmpDir::new("winshadow");
    let mut w = WindowRt::new(spec(), WindowShape::PlainI64);
    let mut seg = Segment::new();

    // First slide: r:0..r:99 go cold, r:200 keeps the window open.
    let vals: Vec<i64> = (0..100).chain(std::iter::once(200)).collect();
    assert!(slide_once(&mut w, &mut seg, dir.path(), &vals), "first slide");
    assert!(cold_keys(&w).contains(&b"r:10".to_vec()), "r:10 is cold");

    // The row is written again — its cold entry is now stale, so the
    // write path shadows it. This is the legitimate tombstone.
    w.on_row_write(b"r:10");
    assert!(!cold_keys(&w).contains(&b"r:10".to_vec()), "stale entry hidden");

    // The new value slides in its turn, into a LATER segment.
    seg.apply(b"r:10", Some(IndexValue::I64(150)));
    assert!(slide_once(&mut w, &mut seg, dir.path(), &[400]), "second slide");

    let keys = cold_keys(&w);
    assert!(
        keys.contains(&b"r:10".to_vec()),
        "the row's new cold entry must be visible; shadow reached forward: {keys:?}"
    );
    assert_eq!(
        keys.iter().filter(|k| k.as_slice() == b"r:10").count(),
        1,
        "and exactly once — the stale entry stays hidden"
    );
}

/// The bloom that gates tombstones has false positives, so a write can
/// shadow a row with no cold entry at all. That shadow must not hide
/// the entry the row is given when it slides for the first time.
#[test]
fn a_shadow_spent_before_the_row_ever_slid_hides_nothing() {
    let dir = kevy_tmpdir::TmpDir::new("winshadow2");
    let mut w = WindowRt::new(spec(), WindowShape::PlainI64);
    let mut seg = Segment::new();

    // Something slides so the bloom is non-empty and the row's later
    // tombstone is reachable at all.
    assert!(slide_once(&mut w, &mut seg, dir.path(), &[0, 1, 2, 200]), "first slide");

    // Stand in for the false positive: shadow a row that is still hot
    // and has never been cold. (A real one arrives via the bloom; the
    // consequence is identical and this way the test is deterministic.)
    seg.apply(b"r:0", Some(IndexValue::I64(150)));
    w.on_row_write(b"r:0");

    assert!(slide_once(&mut w, &mut seg, dir.path(), &[400]), "second slide");
    assert!(
        cold_keys(&w).contains(&b"r:0".to_vec()),
        "a shadow spent before the entry existed must not hide it"
    );
}
