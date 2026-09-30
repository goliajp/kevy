//! The cold side's bookkeeping: what a slide leaves on disk, how a
//! failure reads, and what the audit counts.

use std::error::Error as _;

use kevy_index::{IndexValue, Segment, ValType, WindowShape, WindowSpec};
use kevy_text::{CorpusStats, Distinct, SegmentShape, Sort, SortOrder, TextSegment};

use crate::{ColdError, ColdPageQuery, TextColdDir, WindowRt};

fn window() -> WindowRt {
    WindowRt::new(WindowSpec::new("ts", 100, 50), WindowShape::PlainI64)
}

fn tree(vals: &[i64]) -> Segment {
    let mut seg = Segment::new();
    for v in vals {
        seg.apply(format!("r:{v}").as_bytes(), None, Some(IndexValue::I64(*v)));
    }
    seg
}

fn live_cold_files(dir: &std::path::Path) -> Vec<String> {
    let m = kevy_seg::Manifest::open(dir).expect("manifest");
    m.live().map(|e| e.file.clone()).collect()
}

#[test]
fn an_empty_tree_is_an_idle_tick() {
    let dir = kevy_tmpdir::TmpDir::new("winidle");
    let mut w = window();
    assert!(!w.slide(b"t.ts", &mut Segment::new(), dir.path()).expect("slide"));
    assert!(!w.slide(b"t.ts", &mut Segment::new(), dir.path()).expect("slide"));
    assert_eq!(w.idle_ticks(), 2);
}

#[test]
fn a_boundary_with_nothing_below_it_moves_without_writing_a_segment() {
    let dir = kevy_tmpdir::TmpDir::new("winempty");
    let mut w = window();
    assert!(w.audit(ValType::I64).is_none(), "nothing has moved yet");
    let mut seg = tree(&[300]);
    assert!(!w.slide(b"t.ts", &mut seg, dir.path()).expect("slide"));
    assert_eq!(w.idle_ticks(), 0, "the boundary moved, so the tick was not idle");
    let a = w.audit(ValType::I64).expect("the boundary moved");
    assert_eq!((a.boundary, a.cold_live), (200, 0));
    assert_eq!(seg.stats().entries, 1);
    assert!(std::fs::read_dir(dir.path()).expect("dir").next().is_none(), "no file was written");
}

#[test]
fn the_audit_counts_cold_entries_and_leaves_out_shadowed_rows() {
    let dir = kevy_tmpdir::TmpDir::new("winaudit");
    let mut w = window();
    let mut seg = tree(&[10, 20, 30, 300]);
    assert!(w.slide(b"t.ts", &mut seg, dir.path()).expect("slide"));
    assert_eq!(w.audit(ValType::I64).expect("slid").cold_live, 3);

    w.on_row_write(b"r:20");
    let a = w.audit(ValType::I64).expect("slid");
    assert_eq!((a.boundary, a.shape, a.cold_live), (200, WindowShape::PlainI64, 2));
}

#[test]
fn a_restart_drops_the_previous_runs_segments_for_the_same_index() {
    let dir = kevy_tmpdir::TmpDir::new("winrestart");
    let mut first = window();
    assert!(first.slide(b"t.ts", &mut tree(&[1, 2, 300]), dir.path()).expect("slide"));
    assert!(first.slide(b"t.ts", &mut tree(&[250, 400]), dir.path()).expect("slide"));
    assert_eq!(live_cold_files(dir.path()).len(), 2);

    let mut second = window();
    assert!(second.slide(b"t.ts", &mut tree(&[5, 300]), dir.path()).expect("slide"));
    let live = live_cold_files(dir.path());
    assert_eq!(live.len(), 1, "the earlier run's two segments are unregistered: {live:?}");
    let on_disk = std::fs::read_dir(dir.path())
        .expect("dir")
        .filter(|e| e.as_ref().expect("entry").path().extension().is_some_and(|x| x == "seg"))
        .count();
    assert_eq!(on_disk, 1, "and their files unlinked");
    assert_eq!(second.audit(ValType::I64).expect("slid").cold_live, 1);
}

#[test]
fn a_segment_directory_that_cannot_be_created_is_an_io_error_and_the_tree_stays() {
    let dir = kevy_tmpdir::TmpDir::new("winioerr");
    let file = dir.path().join("plain-file");
    std::fs::write(&file, b"").expect("write");
    let mut w = window();
    let mut seg = tree(&[10, 300]);
    let e = w.slide(b"t.ts", &mut seg, &file.join("segs")).unwrap_err();
    let ColdError::Io(io) = &e else { panic!("expected Io, got {e:?}") };
    assert_eq!(e.to_string(), io.to_string());
    let src = e.source().expect("io source");
    assert!(src.downcast_ref::<std::io::Error>().is_some());
    assert_eq!(seg.stats().entries, 2);
    assert!(w.audit(ValType::I64).is_none(), "the boundary did not move");
}

#[test]
fn a_segment_directory_whose_manifest_will_not_open_is_a_seg_error() {
    let dir = kevy_tmpdir::TmpDir::new("winsegerr");
    let file = dir.path().join("plain-file");
    std::fs::write(&file, b"").expect("write");
    let mut w = window();
    let mut seg = tree(&[10, 300]);
    let e = w.slide(b"t.ts", &mut seg, &file).unwrap_err();
    let ColdError::Seg(inner) = &e else { panic!("expected Seg, got {e:?}") };
    assert_eq!(e.to_string(), inner.to_string());
    let src = e.source().expect("seg source");
    assert!(src.downcast_ref::<kevy_seg::SegError>().is_some());
    assert_eq!(seg.stats().entries, 2);
}

#[test]
fn every_cold_error_renders_its_cause_and_chains_its_source() {
    let reopen = ColdError::Reopen {
        file: "idx-74-0.seg".into(),
        source: kevy_seg::SegError::Corrupt("footer"),
    };
    assert_eq!(reopen.to_string(), "reopen idx-74-0.seg: corrupt segment: footer");
    assert_eq!(
        reopen.source().map(ToString::to_string).as_deref(),
        Some("corrupt segment: footer")
    );

    let seg = ColdError::from(kevy_seg::SegError::Unsorted);
    assert!(matches!(seg, ColdError::Seg(kevy_seg::SegError::Unsorted)));
    assert_eq!(seg.to_string(), "keys must be strictly ascending");
    assert_eq!(
        seg.source().map(ToString::to_string).as_deref(),
        Some("keys must be strictly ascending")
    );

    assert_eq!(ColdError::CorruptKey.to_string(), "corrupt cold key");
    assert_eq!(ColdError::CorruptPayload.to_string(), "corrupt cold payload");
    assert!(ColdError::CorruptKey.source().is_none());
    assert!(ColdError::CorruptPayload.source().is_none());
}

#[test]
fn a_cold_page_honours_sort_and_distinct_clauses() {
    let dir = kevy_tmpdir::TmpDir::new("wincoldpage");
    let mut ts = TextSegment::with_shape(SegmentShape::default().with_values(1));
    let docs = [
        ("d:1", "red apple", "red"),
        ("d:2", "green apple", "green"),
        ("d:3", "ripe apple", "red"),
    ];
    for (key, text, colour) in docs {
        ts.apply_doc(
            key.as_bytes(),
            Some(&[(text.as_bytes().to_vec(), 1.0)]),
            &[Some(colour.as_bytes())],
        );
    }
    let mut cold = TextColdDir::new();
    let keys: Vec<Vec<u8>> = docs.iter().map(|d| d.0.as_bytes().to_vec()).collect();
    assert!(cold.freeze_batch(&mut ts, b"t.body", &keys, dir.path()).expect("freeze"));
    let stats = CorpusStats::new(3.0, 2.0, Default::default());
    let colour = |v: &[u8]| Some(v.to_vec());

    let desc = Sort::new(0, &colour).with_order(SortOrder::Desc);
    let page = cold.cold_page(&ColdPageQuery::parse(b"apple", &stats, 10).with_sort(&desc));
    let okeys: Vec<Option<&[u8]>> = page.hits.iter().map(|h| h.okey.as_deref()).collect();
    assert_eq!(okeys, [Some(&b"red"[..]), Some(b"red"), Some(b"green")]);

    let per_colour = Distinct::new(0, &colour);
    let page =
        cold.cold_page(&ColdPageQuery::parse(b"apple", &stats, 10).with_distinct(&per_colour));
    assert_eq!(page.hits.len(), 2, "one hit per colour: {:?}", page.hits);
}
