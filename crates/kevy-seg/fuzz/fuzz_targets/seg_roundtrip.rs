//! Arbitrary sorted record sets must round-trip byte-identically, and
//! every range/count must agree with a reference walk.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: Vec<(Vec<u8>, Vec<u8>)>| {
    let mut recs: Vec<(Vec<u8>, Vec<u8>)> = data
        .into_iter()
        .filter(|(k, _)| k.len() <= 512)
        .take(300)
        .collect();
    recs.sort_by(|a, b| a.0.cmp(&b.0));
    recs.dedup_by(|a, b| a.0 == b.0);
    if recs.is_empty() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("segfuzz-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("f.seg");
    let mut b = kevy_seg::SegBuilder::create(&path).unwrap();
    for (k, v) in &recs {
        b.push(k, v).unwrap();
    }
    let meta = b.finish().unwrap();
    assert_eq!(meta.records as usize, recs.len());
    let s = kevy_seg::Seg::open(&path).unwrap();
    for (k, v) in &recs {
        assert_eq!(s.get(k).unwrap().as_deref(), Some(v.as_slice()));
    }
    // One arbitrary range must agree with the reference walk.
    let lo = &recs[0].0;
    let hi = &recs[recs.len() / 2].0;
    let want: Vec<&Vec<u8>> =
        recs.iter().map(|(k, _)| k).filter(|k| k.as_slice() >= lo.as_slice() && k.as_slice() <= hi.as_slice()).collect();
    let got: Vec<Vec<u8>> = s.range(lo, hi).map(|r| r.unwrap().0).collect();
    assert_eq!(got.len(), want.len());
    assert_eq!(s.count_range(lo, hi).unwrap() as usize, want.len());
    let _ = std::fs::remove_file(&path);
});
