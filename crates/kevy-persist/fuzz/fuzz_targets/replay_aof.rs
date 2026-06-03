//! Fuzz `kevy_persist::replay_aof` on arbitrary byte streams.
//!
//! Motivated by the mailrs 2026-06-03 incident: pre-1.1.1, the replay
//! path silently `break`'d on any parse error, so prod restarts after a
//! poisoned AOF came up with an empty store and no log line. Post-1.1.1
//! the path emits a loud summary line; this target asserts the post-1.1.1
//! invariant — replay must terminate, must not panic, must not OOM —
//! across arbitrary inputs, INCLUDING:
//!
//!   * empty files
//!   * pure RESP streams
//!   * pure inline-form streams (the RESP "raw text" fallback)
//!   * non-kevy bytes accidentally prepended (the original incident shape)
//!   * v1.2.0 AOF_MAGIC prefix + arbitrary tail
//!   * malformed multibulk length headers
//!   * arbitrary corruption mid-stream

#![no_main]

use libfuzzer_sys::fuzz_target;
use std::io::Write;

fuzz_target!(|data: &[u8]| {
    // Write to a process-and-input-derived temp path so concurrent fuzz
    // workers don't race on the same file.
    let path = std::env::temp_dir().join(format!(
        "kevy-persist-fuzz-{}-{}.aof",
        std::process::id(),
        // Cheap input-derived suffix to avoid collisions inside one
        // worker between successive iterations.
        data.iter().fold(0u64, |a, b| a.wrapping_mul(31).wrapping_add(*b as u64)),
    ));
    {
        let Ok(mut f) = std::fs::File::create(&path) else {
            return;
        };
        let _ = f.write_all(data);
    }

    // The asserted property: replay must terminate without panicking
    // regardless of file contents. The summary line goes to stderr,
    // which libfuzzer captures but doesn't fail on. `apply` is a
    // no-op closure — we only care about the parse path's totality.
    let mut count: u64 = 0;
    let _ = kevy_persist::replay_aof(&path, |_| count = count.saturating_add(1));

    let _ = std::fs::remove_file(&path);
});
