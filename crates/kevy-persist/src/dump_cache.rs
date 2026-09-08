//! Page-cache hygiene for the streamed rewrite image — split from
//! `rewrite_fmt.rs` at the 500-LOC line. See the drop-behind note in
//! [`crate::dump_aof`].

// A discarded fsync. A transient failure self-heals — `dirty` stays
// set and the next tick retries — but a persistent one (full disk,
// read-only remount, EIO) means `appendfsync everysec` has quietly
// become "never" with nothing saying so. Open question §2.
#![expect(
    clippy::let_underscore_must_use,
    reason = "a persistent fsync failure is invisible; see .claude/OPEN-QUESTIONS-6.4.md"
)]

use std::fs::File;
use std::io::{BufWriter, Write};

/// One drop-behind stride: flush, fdatasync, drop the clean cache.
pub(crate) fn drop_behind_step(w: &mut BufWriter<File>) {
    if w.flush().is_ok() {
        let file = w.get_ref();
        let _ = file.sync_data();
        drop_cache_all(file);
    }
}

/// Best-effort page-cache drop for a synced streamed file (Linux
/// fadvise; no-op elsewhere). See the drop-behind note in `dump_aof`.
fn drop_cache_all(f: &File) {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let _ = kevy_sys::fadvise_dontneed_all(f.as_raw_fd());
    }
    #[cfg(not(unix))]
    let _ = f;
}
