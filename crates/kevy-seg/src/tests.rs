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
        .map(|i| {
            (
                format!("k{i:08}").into_bytes(),
                format!("v{i}-{}", "x".repeat(i as usize % 90)).into_bytes(),
            )
        })
        .collect();
    let meta = build(&p, recs.iter().map(|(k, v)| (k.as_slice(), v.as_slice())));
    assert_eq!(meta.records, 5000);
    assert!(meta.data_pages > 1, "must have paged");

    let s = Seg::open(&p).expect("open");
    assert_eq!(s.meta().records, 5000);
    assert_eq!(s.meta().min_key, b"k00000000".to_vec());
    // Every record comes back byte-identical.
    for (k, v) in &recs {
        assert_eq!(
            s.get(k).expect("get").as_deref(),
            Some(v.as_slice()),
            "{}",
            String::from_utf8_lossy(k)
        );
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
    let walked: Vec<Vec<u8>> = s.range(lo, hi).map(|r| r.expect("range record").0).collect();
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

    /// A bit flipped in a record's LENGTH must not be read as a torn tail.
    ///
    /// The CRC covers the body; the four length bytes sit outside it. So a
    /// flip there makes the record undecodable, and an undecodable record
    /// used to be taken for a crash mid-append — which truncates everything
    /// after it. Measured before this: flipping one bit in the FIRST
    /// record's length took a five-entry, 220-byte ledger to zero entries
    /// and zero bytes, with `open()` returning `Ok`. `sweep()` then deletes
    /// every segment file the ledger no longer names, so that is real data
    /// gone from disk, from one bit, with no error anywhere.
    ///
    /// The last record is the one case that cannot be decided from the
    /// bytes: nothing follows it, so a wrong length there really is
    /// indistinguishable from a tail. Losing that one record is what a torn
    /// tail would have cost anyway. Every earlier record is now decidable
    /// and is refused by name.
    #[test]
    fn a_flipped_length_is_refused_rather_than_read_as_a_torn_tail() {
        let d = kevy_tmpdir::TmpDir::new("man-lenflip");
        {
            let mut m = Manifest::open(d.path()).unwrap();
            for i in 0..5 {
                m.add(entry(&format!("s{i}.seg"), i)).unwrap();
            }
        }
        let path = d.path().join("segs.manifest");
        let good = std::fs::read(&path).unwrap();
        let full = Manifest::open(d.path()).unwrap().live().count();
        assert_eq!(full, 5);

        // Walk the real record boundaries.
        let mut offs = Vec::new();
        let mut o = 0usize;
        while o + 8 <= good.len() {
            let len = u32::from_le_bytes(good[o..o + 4].try_into().expect("4 bytes")) as usize;
            offs.push(o);
            o += 8 + len;
        }
        assert_eq!(offs.len(), 5, "expected five records, found {}", offs.len());
        let last = *offs.last().expect("five records");

        let mut silent = Vec::new();
        for &ro in &offs {
            for byte in 0..4usize {
                for bit in 0..8u8 {
                    let mut bad = good.clone();
                    bad[ro + byte] ^= 1 << bit;
                    std::fs::write(&path, &bad).unwrap();
                    if let Ok(m) = Manifest::open(d.path())
                        && m.live().count() != full
                    {
                        silent.push((ro, byte, bit));
                    }
                }
            }
        }
        let escaped: Vec<_> = silent.iter().filter(|(ro, _, _)| *ro != last).collect();
        assert!(
            escaped.is_empty(),
            "{} flips outside the last record silently dropped entries: {escaped:?}",
            escaped.len()
        );
    }

    /// And a genuine torn tail still recovers — every prefix of a real
    /// envelope, not just a hand-made one.
    ///
    /// The first version of the check above refused a tail shorter than the
    /// length field itself, which is the most ordinary crash there is. This
    /// is what caught that.
    #[test]
    fn every_prefix_of_an_interrupted_append_is_still_recovered() {
        let d = kevy_tmpdir::TmpDir::new("man-prefix");
        {
            let mut m = Manifest::open(d.path()).unwrap();
            for i in 0..3 {
                m.add(entry(&format!("s{i}.seg"), i)).unwrap();
            }
        }
        let path = d.path().join("segs.manifest");
        let good = std::fs::read(&path).unwrap();

        // A real fourth envelope, produced by the writer itself.
        let scratch = kevy_tmpdir::TmpDir::new("man-prefix-src");
        let six = {
            let mut m = Manifest::open(scratch.path()).unwrap();
            for i in 0..3 {
                m.add(entry(&format!("s{i}.seg"), i)).unwrap();
            }
            m.add(entry("s3.seg", 3)).unwrap();
            std::fs::read(scratch.path().join("segs.manifest")).unwrap()
        };
        let envelope = &six[good.len()..];
        assert!(envelope.len() > 8, "the fourth record must be a real envelope");

        for cut in 1..envelope.len() {
            let mut torn = good.clone();
            torn.extend_from_slice(&envelope[..cut]);
            std::fs::write(&path, &torn).unwrap();
            let m = Manifest::open(d.path())
                .unwrap_or_else(|e| panic!("a {cut}-byte torn tail was refused: {e}"));
            assert_eq!(m.live().count(), 3, "torn tail of {cut} bytes lost an entry");
        }
    }

    /// `append` refuses a record larger than the reader is willing to
    /// believe, because that bound is exactly what lets `replay` tell a
    /// damaged length from a torn tail. A writer that could exceed it
    /// would be writing something the reader must later call corrupt.
    #[test]
    fn a_record_too_large_for_the_reader_is_refused_by_the_writer() {
        let d = kevy_tmpdir::TmpDir::new("man-toobig");
        let mut m = Manifest::open(d.path()).unwrap();
        let mut e = entry("huge.seg", 1);
        e.meta = vec![0u8; 128 * 1024];
        let err = m.add(e).expect_err("a record past the reader's bound must be refused");
        assert!(format!("{err}").contains("too large"), "the refusal must say why, got: {err}");
        // And the ledger is untouched — a refused write leaves nothing.
        drop(m);
        assert_eq!(Manifest::open(d.path()).unwrap().live().count(), 0);
    }
}

/// A count read out of a file is a claim, not a size.
///
/// The footer's fence count and an overflow cell's `total_len` both reach
/// `Vec::with_capacity` straight from the bytes. Measured before the clamp:
/// a 28-byte footer body claiming `u32::MAX` fences reserved 131,071 MB —
/// on this platform the request was served out of virtual address space and
/// the decode still returned `None`, so nothing failed visibly; a platform
/// that refuses the request aborts the process instead. `kevy-persist`
/// clamps its snapshot counts for exactly this reason and says so.
#[test]
fn a_count_from_a_file_cannot_size_an_allocation() {
    use crate::layout::{RESERVE_CAP, capped_capacity};
    assert_eq!(capped_capacity(3), 3, "an honest small count is used as-is");
    assert_eq!(capped_capacity(RESERVE_CAP), RESERVE_CAP, "the bar itself passes");
    assert_eq!(
        capped_capacity(u32::MAX as usize),
        RESERVE_CAP,
        "a forged count reserves the cap, not the claim"
    );
}

/// The clamp must not change what a lying footer is answered with.
#[test]
fn a_lying_fence_count_is_still_refused() {
    let nf: u32 = u32::MAX;
    let mut b = Vec::new();
    b.extend_from_slice(&1u64.to_le_bytes()); // records
    b.extend_from_slice(&1u32.to_le_bytes()); // data_pages
    b.extend_from_slice(&0u32.to_le_bytes()); // min_key len
    b.extend_from_slice(&0u32.to_le_bytes()); // max_key len
    b.extend_from_slice(&nf.to_le_bytes());
    let crc = kevy_sys::checksum::crc32c(&b);
    b.extend_from_slice(&crc.to_le_bytes());
    assert!(
        crate::layout::decode_footer(&b).is_none(),
        "the CRC is over the lie, so only the body-exhausted check catches it"
    );

    // And an honest footer still round-trips, so the refusal is not blanket.
    let fences = vec![(0u32, b"aa".to_vec()), (7u32, b"zz".to_vec())];
    let enc = crate::layout::encode_footer(42, 3, b"aa", b"zz", &fences);
    let got = crate::layout::decode_footer(&enc).expect("honest footer decodes");
    assert_eq!(got, (42u64, 3u32, b"aa".to_vec(), b"zz".to_vec(), fences));
}

/// Every key length around the page boundary either stores and reads
/// back, or is refused — never panics, and never reports a write that
/// cannot be read.
///
/// Measured before the bound existed, one key with a 7-byte payload:
///
/// | key bytes | result |
/// |---|---|
/// | ≤ 4070 | stored, read back |
/// | 4074 | `push` Ok, `finish` Ok, `open` Ok, **read back fails** |
/// | ≥ 4075 | **panic** |
///
/// The middle row is the worse one: the slot directory is written over
/// the tail of the cell and the page CRC is taken afterwards, so the
/// page is internally consistent and wrong, and the builder reports
/// success. Both are reachable from user data — a document with a long
/// run of non-separator bytes becomes one token and then one key.
#[test]
fn no_key_length_panics_or_writes_something_unreadable() {
    let d = kevy_tmpdir::TmpDir::new("seg-keylen");
    let mut stored = 0usize;
    let mut refused = 0usize;
    for klen in (1..64).chain(4000..4200).chain([8192, 65_536]) {
        let path = d.path().join(format!("k{klen}.seg"));
        let key = vec![b'k'; klen];
        let mut b = match SegBuilder::create(&path) {
            Ok(b) => b,
            Err(e) => panic!("create failed at klen {klen}: {e}"),
        };
        if b.push(&key, b"payload").is_err() {
            refused += 1;
            continue;
        }
        b.finish().unwrap_or_else(|e| panic!("finish failed at klen {klen}: {e}"));
        let seg = Seg::open(&path).unwrap_or_else(|e| panic!("open failed at klen {klen}: {e}"));
        let got = seg
            .get(&key)
            .unwrap_or_else(|e| panic!("a segment written at klen {klen} cannot be read: {e}"));
        assert!(got.is_some(), "key of {klen} bytes stored but not found");
        stored += 1;
    }
    // Floors: a sweep that stored nothing, or refused nothing, would
    // satisfy the assertions above without exercising either side.
    assert!(stored > 60, "only {stored} key lengths stored — the sweep collapsed");
    assert!(refused > 0, "no key length was refused — the bound is not being hit");
}

/// A page header that lies about its slot count, with the CRC recomputed
/// so the existing check agrees with it.
///
/// `page_intact` answers "these bytes did not change". It cannot answer
/// "these bytes describe a page", and `n_slots` lives inside the CRC's
/// range, so anything that rewrites the page — a repair tool, a restore
/// from a stale copy, anyone who can write the data directory — can hand
/// the reader a self-consistent page claiming 65535 slots. The slot array
/// grows backward from the CRC, so slot 65534 sits at
/// `4096 - 4 - 2 * 65535`, which is not a place.
#[test]
fn a_page_that_claims_more_slots_than_fit_is_corrupt_not_a_panic() {
    use std::io::{Read, Seek, SeekFrom, Write};

    let (_d, p) = tmp("seg-slotcount");
    build(&p, [(&b"a"[..], &b"1"[..]), (&b"b"[..], &b"2"[..])]);

    let mut f = std::fs::OpenOptions::new().read(true).write(true).open(&p).expect("reopen");
    let mut page = vec![0u8; crate::layout::PAGE];
    f.read_exact(&mut page).expect("read page 0");
    page[0..2].copy_from_slice(&u16::MAX.to_le_bytes());
    let crc = kevy_sys::checksum::crc32c(&page[..crate::layout::PAGE - crate::layout::PAGE_CRC]);
    let n = crate::layout::PAGE - crate::layout::PAGE_CRC;
    page[n..].copy_from_slice(&crc.to_le_bytes());
    f.seek(SeekFrom::Start(0)).expect("seek");
    f.write_all(&page).expect("rewrite page 0");
    f.sync_all().expect("sync");
    drop(f);

    let seg = Seg::open(&p).expect("the footer and trailer are untouched");
    assert!(
        matches!(seg.get(b"a"), Err(SegError::Corrupt(_))),
        "a page whose slot count cannot fit must be refused as corrupt"
    );
}

/// The bound must not reject a page the builder can actually write.
///
/// A guard set one slot too tight would refuse real segments, and the
/// corruption test above would still pass — it only proves the guard fires,
/// not that it fires on the right side. Thousands of two-byte records pack
/// the densest pages this format produces; every one of them must read back.
#[test]
fn the_densest_pages_the_builder_writes_are_within_the_slot_bound() {
    let (_d, p) = tmp("seg-dense");
    let recs: Vec<(Vec<u8>, Vec<u8>)> =
        (0..4000u32).map(|i| (format!("{i:06}").into_bytes(), vec![b'v'])).collect();
    let meta = build(&p, recs.iter().map(|(k, v)| (&k[..], &v[..])));
    assert!(meta.data_pages > 1, "a single page would not exercise the bound");

    let seg = Seg::open(&p).expect("open");
    for (k, v) in &recs {
        assert_eq!(seg.get(k).expect("get"), Some(v.clone()), "key {k:?}");
    }
    let walked = seg.range(b"000000", b"999999").count();
    assert_eq!(walked, recs.len(), "every record walks back");
}

/// Cell offsets are `usize` built from a `u16` and a `u32` read off disk,
/// and on a 32-bit target — `armv7-unknown-linux-musleabihf` and
/// `thumbv7em-none-eabihf` are both in CI's matrix — `off + 6 + klen + plen`
/// with a `plen` near `u32::MAX` runs past the end of `usize`. Debug panics
/// on the addition; release wraps to an end below the start, and the slice
/// lookup then returns `None` for the wrong reason.
///
/// A 64-bit host cannot reach that through a page, so the arithmetic is a
/// pure function and the test hands it an offset directly.
#[test]
fn a_cell_offset_that_runs_off_the_end_of_usize_is_none_not_a_panic() {
    use crate::layout::{field_end, read_cell};

    assert_eq!(field_end(10, 6), Some(16), "ordinary arithmetic is unchanged");
    assert_eq!(field_end(usize::MAX, 1), None);
    assert_eq!(field_end(usize::MAX - 1, 2), None, "landing exactly past the end");
    assert_eq!(field_end(usize::MAX - 2, 2), Some(usize::MAX), "landing exactly on it");

    let page = vec![0u8; crate::layout::PAGE];
    assert!(read_cell(&page, usize::MAX - 1).is_none(), "no panic, no cell");
}

/// An overflow cell names its payload run as `first_page` + `n_pages`, both
/// `u32` and both read off disk. A run pointing past the end of the file
/// must fail the read rather than return whatever is there.
///
/// This started out claiming to catch a `u32` overflow in `first_page + p`,
/// and mutation testing said otherwise: replacing the widened addition with
/// a wrapping one leaves this green. The reason is worth keeping — reaching
/// `p >= 1` means the read at `p == 0` succeeded, so `first_page` names a
/// page the file has, and a file with `u32::MAX` pages is 17.6 TB. The
/// overflow is unreachable, the widening is there so that stops being
/// something a reader has to work out, and this test verifies the thing it
/// can actually verify.
#[test]
fn an_overflow_run_pointing_past_the_file_is_refused() {
    use crate::layout::{self, Cell};
    use std::io::{Read, Seek, SeekFrom, Write};

    let (_d, p) = tmp("seg-ovf-wrap");
    build(&p, [(b"k".as_slice(), vec![b'X'; 9000].as_slice())]);

    let mut f = std::fs::OpenOptions::new().read(true).write(true).open(&p).expect("reopen");
    let pages = f.metadata().expect("metadata").len() / layout::PAGE as u64;

    // The payload pages come first, so page 0 is raw `X`s — its first two
    // bytes read as a slot count of 22616. Find the data page by shape.
    let mut patched = None;
    for ix in 0..pages {
        let mut page = vec![0u8; layout::PAGE];
        f.seek(SeekFrom::Start(ix * layout::PAGE as u64)).expect("seek");
        if f.read_exact(&mut page).is_err() {
            break;
        }
        if !layout::page_intact(&page) || !layout::page_shape_ok(&page) {
            continue;
        }
        let mut found = false;
        for s in 0..layout::page_slots(&page) {
            let off = layout::slot_offset(&page, s);
            let Some(Cell::Overflow { key, .. }) = layout::read_cell(&page, off) else { continue };
            let tail = off + 6 + key.len();
            // Point the run at the top of the u32 range, two pages long, so
            // the second page's index is `first_page + 1`.
            page[tail + 4..tail + 8].copy_from_slice(&u32::MAX.to_le_bytes());
            page[tail + 8..tail + 12].copy_from_slice(&2u32.to_le_bytes());
            found = true;
            break;
        }
        if found {
            let n = layout::PAGE - layout::PAGE_CRC;
            let crc = kevy_sys::checksum::crc32c(&page[..n]);
            page[n..].copy_from_slice(&crc.to_le_bytes());
            f.seek(SeekFrom::Start(ix * layout::PAGE as u64)).expect("seek");
            f.write_all(&page).expect("rewrite");
            patched = Some(ix);
            break;
        }
    }
    assert!(patched.is_some(), "the 9000-byte value must have produced an overflow cell");
    f.sync_all().expect("sync");
    drop(f);

    let seg = Seg::open(&p).expect("footer untouched");
    // Named precisely, not just `is_err()`. Two independent things refuse
    // this run — the read fails, and a zero-filled buffer fails the page CRC
    // — so `is_err()` stays true with either one removed and proves neither.
    // The read is the one that should fire first.
    assert!(
        matches!(seg.get(b"k"), Err(SegError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof),
        "a run past the end of the file must fail the read, not be filled in"
    );
}
