//! What each error says, what it chains to, and the store paths that
//! raise the row-segment ones.

use std::error::Error as _;

use kevy_seg::{Manifest, ManifestEntry, SegBuilder, SegError, SegMeta};

use crate::StreamIdError;
use crate::{KevyError, SegRowsError, Store, StoreError};

#[test]
fn every_store_error_has_its_wire_line_and_its_message() {
    let cases = [
        (
            StoreError::WrongType,
            "WRONGTYPE Operation against a key holding the wrong kind of value",
            "wrong type for this operation",
        ),
        (
            StoreError::NotInteger,
            "ERR value is not an integer or out of range",
            "value is not an integer or out of range",
        ),
        (
            StoreError::Overflow,
            "ERR increment or decrement would overflow",
            "increment or decrement would overflow",
        ),
        (StoreError::OutOfRange, "ERR index out of range", "index out of range"),
        (StoreError::NoSuchKey, "ERR no such key", "no such key"),
        (StoreError::NotFloat, "ERR value is not a valid float", "value is not a valid float"),
        (
            StoreError::ScoreIsNan,
            "ERR resulting score is not a number (NaN)",
            "resulting score is not a number (NaN)",
        ),
        (
            StoreError::OutOfMemory,
            "OOM command not allowed when used memory > 'maxmemory'.",
            "maxmemory reached and the eviction policy is noeviction",
        ),
    ];
    for (e, wire, text) in cases {
        assert_eq!(e.as_wire(), wire);
        assert_eq!(e.to_string(), text);
        assert!(e.source().is_none());
    }
}

#[test]
fn every_kevy_error_says_what_it_is_and_chains_its_cause() {
    let store = KevyError::from(StoreError::NoSuchKey);
    assert_eq!(store.to_string(), "store error: no such key");
    assert_eq!(store.source().map(ToString::to_string).as_deref(), Some("no such key"));

    let io = KevyError::from(std::io::Error::other("disk gone"));
    assert_eq!(io.to_string(), "io error: disk gone");
    assert_eq!(io.source().map(ToString::to_string).as_deref(), Some("disk gone"));

    let leaves = [
        (KevyError::Protocol("bad frame".into()), "protocol error: bad frame"),
        (KevyError::ReadOnly, "write refused: this is a read-only replica"),
        (KevyError::InvalidInput("port".into()), "invalid input: port"),
        (KevyError::NotFound("key".into()), "not found: key"),
        (KevyError::Unsupported("cluster".into()), "unsupported: cluster"),
        (KevyError::TimedOut, "timed out"),
        (KevyError::Closed, "connection closed"),
    ];
    for (e, text) in leaves {
        assert_eq!(e.to_string(), text);
        assert!(e.source().is_none(), "{text}");
    }
}

#[test]
fn a_stream_id_error_names_the_id_as_invalid() {
    assert_eq!(StreamIdError::Invalid.to_string(), "invalid stream id");
}

#[test]
fn every_seg_rows_error_renders_and_chains_its_segment_cause() {
    let corrupt = || SegError::Corrupt("bad footer");
    let io = SegRowsError::Io(std::io::Error::other("no space"));
    assert_eq!(io.to_string(), "no space");
    assert_eq!(io.source().map(ToString::to_string).as_deref(), Some("no space"));

    let seg = SegRowsError::from(corrupt());
    assert!(matches!(seg, SegRowsError::Seg(SegError::Corrupt("bad footer"))));
    assert_eq!(seg.to_string(), "corrupt segment: bad footer");
    assert_eq!(
        seg.source().map(ToString::to_string).as_deref(),
        Some("corrupt segment: bad footer")
    );

    let open = SegRowsError::Open { file: "row-t-0.seg".into(), source: corrupt() };
    assert_eq!(open.to_string(), "open row-t-0.seg: corrupt segment: bad footer");
    assert!(open.source().is_some());

    let reopen = SegRowsError::Reopen { file: "row-t-1.seg".into(), source: corrupt() };
    assert_eq!(reopen.to_string(), "reopen row-t-1.seg: corrupt segment: bad footer");
    assert!(reopen.source().is_some());

    let no_seq = SegRowsError::NoSeq { file: "noseq.seg".into() };
    assert_eq!(no_seq.to_string(), "row segment 'noseq.seg' has no parsable seq");
    assert!(no_seq.source().is_none());

    assert_eq!(SegRowsError::NonUtf8Name.to_string(), "SEGMENTED frame names a non-utf8 segment");
    assert!(SegRowsError::NonUtf8Name.source().is_none());

    let record = SegRowsError::Record { file: "row-t-0.seg".into(), reason: "short row" };
    assert_eq!(record.to_string(), "segment 'row-t-0.seg': short row");
    assert!(record.source().is_none());
}

fn row_manifest(dir: &std::path::Path, file: &str, seg: SegMeta) {
    Manifest::open(dir)
        .unwrap()
        .add(ManifestEntry::new(file, seg).with_meta(b"rowcold:t".to_vec()))
        .unwrap();
}

#[test]
fn enabling_row_segments_refuses_a_damaged_directory() {
    let d = kevy_tmpdir::TmpDir::new("segrows-enable-refusals");

    let file = d.path().join("a-file");
    std::fs::write(&file, b"not a directory").unwrap();
    let err = Store::new().enable_seg_rows(&file).unwrap_err();
    assert!(matches!(err, SegRowsError::Seg(SegError::Io(_))), "{err}");

    let noseq = d.path().join("noseq");
    std::fs::create_dir_all(&noseq).unwrap();
    row_manifest(&noseq, "noseq.seg", SegMeta::default());
    let err = Store::new().enable_seg_rows(&noseq).unwrap_err();
    assert!(matches!(&err, SegRowsError::NoSeq { file } if file == "noseq.seg"), "{err}");

    let gone = d.path().join("gone");
    std::fs::create_dir_all(&gone).unwrap();
    row_manifest(&gone, "row-t-0.seg", SegMeta::default());
    let err = Store::new().enable_seg_rows(&gone).unwrap_err();
    let SegRowsError::Open { file, source } = &err else { panic!("{err}") };
    assert_eq!(file, "row-t-0.seg");
    assert!(matches!(source, SegError::Io(e) if e.kind() == std::io::ErrorKind::NotFound));
}

#[test]
fn a_segmented_frame_is_refused_when_its_segment_cannot_be_stitched() {
    let d = kevy_tmpdir::TmpDir::new("segrows-apply-refusals");

    let err = Store::new().apply_segmented(d.path(), &[0xff, 0xfe]).unwrap_err();
    assert!(matches!(err, SegRowsError::NonUtf8Name));

    let file = d.path().join("a-file");
    std::fs::write(&file, b"not a directory").unwrap();
    let err = Store::new().apply_segmented(&file, b"row-t-0.seg").unwrap_err();
    assert!(matches!(err, SegRowsError::Seg(_)), "{err}");

    let junk = d.path().join("junk");
    std::fs::create_dir_all(&junk).unwrap();
    let mut b = SegBuilder::create(junk.join("row-t-0.seg")).unwrap();
    b.push(b"k", b"junk").unwrap();
    row_manifest(&junk, "row-t-0.seg", b.finish().unwrap());
    let err = Store::new().apply_segmented(&junk, b"row-t-0.seg").unwrap_err();
    let SegRowsError::Record { file, reason } = &err else { panic!("{err}") };
    assert_eq!(file, "row-t-0.seg");
    assert_eq!(err.to_string(), format!("segment 'row-t-0.seg': {reason}"));
}

#[test]
fn a_segmented_frame_stitches_a_row_that_is_still_hot() {
    let d = kevy_tmpdir::TmpDir::new("segrows-apply-hot");
    let mut src = Store::new();
    src.enable_seg_rows(d.path()).unwrap();
    src.hset(b"user:1", &[(b"name".as_slice(), b"ada".as_slice())]).unwrap();
    let sealed = src.seal_rows_to_seg(b"user", &[b"user:1".to_vec()]).unwrap().expect("sealed");
    assert_eq!(sealed.seq(), 0);

    // a replay that reached the row's last write before the frame
    let mut replay = Store::new();
    replay.hset(b"user:1", &[(b"name".as_slice(), b"ada".as_slice())]).unwrap();
    assert_eq!(replay.apply_segmented(d.path(), sealed.file().as_bytes()).unwrap(), 1);
    assert!(matches!(
        replay.map.get(b"user:1".as_slice()).map(|e| &e.value),
        Some(crate::Value::Cold(c)) if c.is_seg()
    ));
    assert_eq!(replay.hget(b"user:1", b"name").unwrap(), Some(&b"ada"[..]));
}
