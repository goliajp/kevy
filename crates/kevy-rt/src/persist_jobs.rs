//! Bodies of the persist worker's rewrite-side jobs — split from
//! `persist_worker.rs` at the 500-LOC line. All run on the worker
//! thread; the drop-behind stride and cache hygiene rationale live in
//! the S5-E/F finding in bench/.

use crate::persist_worker::PersistDone;
use std::io;
use std::path::PathBuf;

/// Best-effort page-cache drop for a fully-synced streamed file.
pub(crate) fn drop_file_cache(f: &std::fs::File) {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let _ = kevy_sys::fadvise_dontneed_all(f.as_raw_fd());
    }
}

/// Append+fsync one tee generation in drop-behind strides (64 MB write
/// → fdatasync → cache drop), so a GB generation never floods the page
/// cache into reclaim; return the buffer cleared for the pool.
pub(crate) fn run_tee_append(tmp: PathBuf, mut bytes: Vec<u8>) -> PersistDone {
    let result = (|| {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&tmp)?;
        for chunk in bytes.chunks(64 << 20) {
            f.write_all(chunk)?;
            f.sync_data()?;
            drop_file_cache(&f);
        }
        f.sync_all()
    })();
    bytes.clear();
    PersistDone::TeeAppend {
        result,
        tmp,
        buf: bytes,
    }
}

/// Fold `[from, to)` of the tee into the image, drop-behind on the
/// destination every 64 MB stride.
pub(crate) fn run_tee_copy(src: &std::fs::File, from: u64, to: u64, tmp: &std::path::Path) -> io::Result<()> {
    use std::io::{Read, Seek, Write};
    let mut dst = std::fs::OpenOptions::new().append(true).open(tmp)?;
    let mut src = src.try_clone()?;
    src.seek(std::io::SeekFrom::Start(from))?;
    let mut left = to - from;
    while left > 0 {
        let stride = left.min(64 << 20);
        let mut take = (&mut src).take(stride);
        std::io::copy(&mut take, &mut dst)?;
        dst.flush()?;
        dst.sync_data()?;
        drop_file_cache(&dst);
        left -= stride;
    }
    dst.sync_all()
}
