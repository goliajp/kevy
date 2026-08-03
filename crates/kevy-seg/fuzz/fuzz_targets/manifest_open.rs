//! Arbitrary bytes handed to Manifest::open must replay, truncate a
//! torn tail, or refuse by name — never panic.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let dir = std::env::temp_dir().join(format!("segfuzz-m-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    if std::fs::write(dir.join("segs.manifest"), data).is_err() {
        return;
    }
    if let Ok(m) = kevy_seg::Manifest::open(&dir) {
        for e in m.live().take(64) {
            let _ = (&e.file, &e.meta, e.records);
        }
    }
    let _ = std::fs::remove_file(dir.join("segs.manifest"));
});
