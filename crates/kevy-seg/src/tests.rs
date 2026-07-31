//! Build → reopen round trips, edge shapes, and corruption refusals.

use crate::{Seg, SegBuilder, SegError};

fn tmp(name: &str) -> (kevy_tmpdir::TmpDir, std::path::PathBuf) {
    let d = kevy_tmpdir::TmpDir::new(name);
    let p = d.path().join("s.seg");
    (d, p)
}

fn build<'a>(
    path: &std::path::Path,
    records: impl IntoIterator<Item = (&'a [u8], &'a [u8])>,
) -> crate::SegMeta {
    let mut b = SegBuilder::create(path).expect("create");
    for (k, v) in records {
        b.push(k, v).expect("push");
    }
    b.finish().expect("finish")
}

#[test]
fn round_trip_across_many_pages() {
    let (_d, p) = tmp("seg-roundtrip");
    let recs: Vec<(Vec<u8>, Vec<u8>)> = (0..5000u32)
        .map(|i| (format!("k{i:08}").into_bytes(), format!("v{i}-{}", "x".repeat(i as usize % 90)).into_bytes()))
        .collect();
    let meta = build(&p, recs.iter().map(|(k, v)| (k.as_slice(), v.as_slice())));
    assert_eq!(meta.records, 5000);
    assert!(meta.data_pages > 1, "must have paged");

    let s = Seg::open(&p).expect("open");
    assert_eq!(s.meta().records, 5000);
    assert_eq!(s.meta().min_key, b"k00000000".to_vec());
    // Every record comes back byte-identical.
    for (k, v) in &recs {
        assert_eq!(s.get(k).expect("get").as_deref(), Some(v.as_slice()), "{}", String::from_utf8_lossy(k));
    }
    // Absent keys answer None on both sides of the fences.
    assert_eq!(s.get(b"k00000000zzz").unwrap(), None);
    assert_eq!(s.get(b"a").unwrap(), None);
    assert_eq!(s.get(b"z").unwrap(), None);
}

#[test]
fn range_and_count_agree_with_the_walk() {
    let (_d, p) = tmp("seg-range");
    let recs: Vec<(Vec<u8>, Vec<u8>)> =
        (0..3000u32).map(|i| (format!("k{i:08}").into_bytes(), vec![b'p'; 10])).collect();
    build(&p, recs.iter().map(|(k, v)| (k.as_slice(), v.as_slice())));
    let s = Seg::open(&p).expect("open");

    let lo = b"k00000100".as_slice();
    let hi = b"k00002599".as_slice();
    let walked: Vec<Vec<u8>> = s
        .range(lo, hi)
        .map(|r| r.expect("range record").0)
        .collect();
    assert_eq!(walked.len(), 2500);
    assert_eq!(walked.first().unwrap().as_slice(), lo);
    assert_eq!(walked.last().unwrap().as_slice(), hi);
    assert!(walked.windows(2).all(|w| w[0] < w[1]), "ascending");
    assert_eq!(s.count_range(lo, hi).expect("count"), 2500);
    assert_eq!(s.count_range(b"a", b"j").unwrap(), 0);
    assert_eq!(s.count_range(b"k00000000", b"kzz").unwrap(), 3000);
}

#[test]
fn overflow_records_round_trip() {
    let (_d, p) = tmp("seg-overflow");
    let big1 = vec![b'A'; 9000]; // > 2 pages
    let big2 = vec![b'B'; 70000]; // many pages
    let meta = build(
        &p,
        [
            (b"a-small".as_slice(), b"tiny".as_slice()),
            (b"b-big".as_slice(), big1.as_slice()),
            (b"c-huge".as_slice(), big2.as_slice()),
            (b"d-after".as_slice(), b"end".as_slice()),
        ],
    );
    assert_eq!(meta.records, 4);
    let s = Seg::open(&p).expect("open");
    assert_eq!(s.get(b"a-small").unwrap().as_deref(), Some(b"tiny".as_slice()));
    assert_eq!(s.get(b"b-big").unwrap().as_deref(), Some(big1.as_slice()));
    assert_eq!(s.get(b"c-huge").unwrap().as_deref(), Some(big2.as_slice()));
    assert_eq!(s.get(b"d-after").unwrap().as_deref(), Some(b"end".as_slice()));
    // Ranges cross the overflow runs without seeing them as pages.
    let all: Vec<_> = s.range(b"a", b"e").map(|r| r.unwrap().0).collect();
    assert_eq!(all.len(), 4);
}

#[test]
fn unsorted_and_duplicate_keys_are_refused() {
    let (_d, p) = tmp("seg-unsorted");
    let mut b = SegBuilder::create(&p).expect("create");
    b.push(b"b", b"1").unwrap();
    assert!(matches!(b.push(b"a", b"2"), Err(SegError::Unsorted)));
    assert!(matches!(b.push(b"b", b"3"), Err(SegError::Unsorted)), "duplicates refused");
}

#[test]
fn corruption_refusals() {
    let (_d, p) = tmp("seg-corrupt2");
    let recs: Vec<(Vec<u8>, Vec<u8>)> =
        (0..800u32).map(|i| (format!("k{i:05}").into_bytes(), vec![b'v'; 40])).collect();
    build(&p, recs.iter().map(|(k, v)| (k.as_slice(), v.as_slice())));

    // Flip a byte in the first data page: open still works (lazy page
    // verify), the touched read refuses by name.
    let mut bytes = std::fs::read(&p).unwrap();
    bytes[100] ^= 0xFF;
    std::fs::write(&p, &bytes).unwrap();
    let s = Seg::open(&p).expect("open is O(footer)");
    match s.get(b"k00000") {
        Err(SegError::Corrupt(w)) => assert!(w.contains("crc"), "{w}"),
        other => panic!("corrupt page served: {other:?}"),
    }

    // Truncate the trailer: open refuses.
    bytes.truncate(bytes.len() - 3);
    std::fs::write(&p, &bytes).unwrap();
    assert!(matches!(Seg::open(&p), Err(SegError::Corrupt(_))));

    // Not a segment at all.
    std::fs::write(&p, b"hello world, definitely not a segment").unwrap();
    assert!(matches!(Seg::open(&p), Err(SegError::Corrupt(_))));
}

#[test]
fn single_record_segment_works() {
    let (_d, p) = tmp("seg-single");
    build(&p, [(b"only".as_slice(), b"one".as_slice())]);
    let s = Seg::open(&p).expect("open");
    assert_eq!(s.meta().records, 1);
    assert_eq!(s.meta().min_key, s.meta().max_key);
    assert_eq!(s.get(b"only").unwrap().as_deref(), Some(b"one".as_slice()));
    assert_eq!(s.count_range(b"a", b"z").unwrap(), 1);
}

mod manifest {
    use crate::{Manifest, ManifestEntry};

    fn entry(file: &str, n: u64) -> ManifestEntry {
        ManifestEntry {
            file: file.to_string(),
            meta: b"table:9/bucket:20260801".to_vec(),
            min_key: b"a".to_vec(),
            max_key: b"z".to_vec(),
            records: n,
        }
    }

    #[test]
    fn ledger_round_trips_across_reopen() {
        let d = kevy_tmpdir::TmpDir::new("man-rt");
        let mut m = Manifest::open(d.path()).expect("open fresh");
        assert_eq!(m.live().count(), 0);
        m.add(entry("s1.seg", 10)).unwrap();
        m.add(entry("s2.seg", 20)).unwrap();
        m.drop_seg("s1.seg").unwrap();
        drop(m);

        let m = Manifest::open(d.path()).expect("reopen");
        let live: Vec<_> = m.live().collect();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0], &entry("s2.seg", 20));
    }

    #[test]
    fn duplicate_add_and_unknown_drop_are_refused() {
        let d = kevy_tmpdir::TmpDir::new("man-dup");
        let mut m = Manifest::open(d.path()).unwrap();
        m.add(entry("s1.seg", 1)).unwrap();
        assert!(m.add(entry("s1.seg", 2)).is_err(), "a name never means two things");
        assert!(m.drop_seg("ghost.seg").is_err());
        // The refusals must not have dirtied the ledger.
        drop(m);
        let m = Manifest::open(d.path()).unwrap();
        assert_eq!(m.live().count(), 1);
    }

    #[test]
    fn torn_tail_is_truncated_but_mid_file_rot_refuses() {
        let d = kevy_tmpdir::TmpDir::new("man-torn");
        let mut m = Manifest::open(d.path()).unwrap();
        m.add(entry("s1.seg", 1)).unwrap();
        m.add(entry("s2.seg", 2)).unwrap();
        drop(m);
        let path = d.path().join("segs.manifest");
        let bytes = std::fs::read(&path).unwrap();

        // Crash mid-append: half a record at the tail is dropped.
        let mut torn = bytes.clone();
        torn.extend_from_slice(&[7, 0, 0, 0, 1, 2, 3]); // len says 7, body absent
        std::fs::write(&path, &torn).unwrap();
        let m = Manifest::open(d.path()).expect("torn tail tolerated");
        assert_eq!(m.live().count(), 2);
        drop(m);
        assert_eq!(std::fs::read(&path).unwrap(), bytes, "tail physically truncated");

        // Bit rot BEFORE the tail is a named refusal, not a truncation.
        let mut rotted = bytes.clone();
        rotted[10] ^= 0xFF;
        std::fs::write(&path, &rotted).unwrap();
        assert!(Manifest::open(d.path()).is_err(), "mid-file rot must refuse");
    }

    #[test]
    fn sweep_deletes_only_unregistered_segments() {
        let d = kevy_tmpdir::TmpDir::new("man-sweep");
        let mut m = Manifest::open(d.path()).unwrap();
        std::fs::write(d.path().join("real.seg"), b"sealed").unwrap();
        std::fs::write(d.path().join("orphan.seg"), b"crash leftover").unwrap();
        std::fs::write(d.path().join("unrelated.txt"), b"not ours").unwrap();
        m.add(entry("real.seg", 5)).unwrap();

        let mut swept = m.sweep(d.path()).unwrap();
        swept.sort();
        assert_eq!(swept, vec!["orphan.seg".to_string()]);
        assert!(d.path().join("real.seg").exists());
        assert!(d.path().join("unrelated.txt").exists());
        assert!(!d.path().join("orphan.seg").exists());
    }

    #[test]
    fn compact_rewrites_to_the_live_set_and_stays_appendable() {
        let d = kevy_tmpdir::TmpDir::new("man-compact");
        let mut m = Manifest::open(d.path()).unwrap();
        for i in 0..20 {
            m.add(entry(&format!("s{i}.seg"), i)).unwrap();
        }
        for i in 0..15 {
            m.drop_seg(&format!("s{i}.seg")).unwrap();
        }
        let before = std::fs::metadata(d.path().join("segs.manifest")).unwrap().len();
        m.compact().unwrap();
        let after = std::fs::metadata(d.path().join("segs.manifest")).unwrap().len();
        assert!(after < before, "dead records gone ({after} !< {before})");
        // Still appendable after the rename swap.
        m.add(entry("post.seg", 99)).unwrap();
        drop(m);
        let m = Manifest::open(d.path()).unwrap();
        assert_eq!(m.live().count(), 6);
        assert!(m.live().any(|e| e.file == "post.seg"));
    }
}
