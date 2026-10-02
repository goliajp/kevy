//! HSCAN / SSCAN / ZSCAN sweeps: every member present for a whole sweep is
//! seen at least once, in every storage form and across a promotion from
//! the flat table to the sharded one in the middle of a sweep.

use std::collections::BTreeSet;

use crate::Store;
use crate::seg_map::HS_PROMOTE;

#[derive(Clone, Copy)]
enum Kind {
    Hash,
    Set,
    ZSet,
}

fn add(s: &mut Store, kind: Kind, from: usize, to: usize) {
    let members: Vec<Vec<u8>> = (from..to).map(|i| format!("m{i}").into_bytes()).collect();
    for chunk in members.chunks(1024) {
        match kind {
            Kind::Hash => {
                let pairs: Vec<(&[u8], &[u8])> =
                    chunk.iter().map(|m| (&m[..], &b"v"[..])).collect();
                s.hset(b"k", &pairs).unwrap();
            }
            Kind::Set => {
                let refs: Vec<&[u8]> = chunk.iter().map(Vec::as_slice).collect();
                s.sadd(b"k", &refs).unwrap();
            }
            Kind::ZSet => {
                let pairs: Vec<(f64, &[u8])> = chunk.iter().map(|m| (1.0, &m[..])).collect();
                s.zadd(b"k", &pairs).unwrap();
            }
        }
    }
}

fn page(s: &mut Store, kind: Kind, cursor: u64, count: usize, seen: &mut BTreeSet<Vec<u8>>) -> u64 {
    match kind {
        Kind::Hash => s.hscan(b"k", cursor, count, |f, _| {
            seen.insert(f.to_vec());
        }),
        Kind::Set => s.sscan(b"k", cursor, count, |m| {
            seen.insert(m.to_vec());
        }),
        Kind::ZSet => s.zscan(b"k", cursor, count, |m, _| {
            seen.insert(m.to_vec());
        }),
    }
    .unwrap()
}

#[test]
fn a_sweep_sees_every_member_in_every_form() {
    for kind in [Kind::Hash, Kind::Set, Kind::ZSet] {
        for n in [3, 300, HS_PROMOTE + 3_000] {
            let mut s = Store::new();
            add(&mut s, kind, 0, n);
            let (mut seen, mut cursor, mut pages) = (BTreeSet::new(), 0, 0);
            loop {
                cursor = page(&mut s, kind, cursor, 37, &mut seen);
                pages += 1;
                if cursor == 0 {
                    break;
                }
            }
            assert_eq!(seen.len(), n, "{n} members");
            // a large one comes in pages, a small one whole
            assert_eq!(pages > 1, n > 128, "{n} members in {pages} pages");
        }
    }
}

#[test]
fn growth_and_promotion_mid_sweep_miss_nothing_present_throughout() {
    for kind in [Kind::Hash, Kind::Set, Kind::ZSet] {
        let mut s = Store::new();
        let start = 2_000;
        add(&mut s, kind, 0, start);
        let (mut seen, mut cursor, mut next) = (BTreeSet::new(), 0, start);
        loop {
            cursor = page(&mut s, kind, cursor, 50, &mut seen);
            if cursor == 0 {
                break;
            }
            // enough writes between pages to promote to the sharded form
            add(&mut s, kind, next, next + 600);
            next += 600;
        }
        assert!(next > HS_PROMOTE, "the sweep crossed the promotion ({next})");
        let missing: Vec<usize> =
            (0..start).filter(|i| !seen.contains(format!("m{i}").as_bytes())).collect();
        assert!(missing.is_empty(), "{} of the first {start} missed", missing.len());
    }
}

#[test]
fn a_cursor_from_another_form_starts_the_sweep_over() {
    let mut s = Store::new();
    add(&mut s, Kind::Set, 0, HS_PROMOTE + 10);
    let mut seen = BTreeSet::new();
    // a flat-table cursor handed to the sharded form
    let mut cursor = 1 << 62;
    loop {
        cursor = page(&mut s, Kind::Set, cursor, 1_000, &mut seen);
        if cursor == 0 {
            break;
        }
    }
    assert_eq!(seen.len(), HS_PROMOTE + 10);
    // and a sharded-form cursor handed to a flat table
    let mut s = Store::new();
    add(&mut s, Kind::Hash, 0, 500);
    let (mut seen, mut cursor) = (BTreeSet::new(), crate::seg_map::SEG_CURSOR | 7);
    loop {
        cursor = page(&mut s, Kind::Hash, cursor, 20, &mut seen);
        if cursor == 0 {
            break;
        }
    }
    assert_eq!(seen.len(), 500);
}
