//! Arbitrary bytes handed to Seg::open must refuse or answer sanely —
//! never panic, never read out of bounds.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let dir = std::env::temp_dir().join(format!("segfuzz-o-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("o.seg");
    if std::fs::write(&path, data).is_err() {
        return;
    }
    if let Ok(s) = kevy_seg::Seg::open(&path) {
        // A structurally valid footer may parse; reads must still never panic.
        let _ = s.get(b"probe");
        let _ = s.count_range(b"", b"\xff\xff\xff\xff");
        for r in s.range(b"", b"\xff\xff\xff\xff").take(64) {
            if r.is_err() {
                break;
            }
        }
    }
    let _ = std::fs::remove_file(&path);
});
