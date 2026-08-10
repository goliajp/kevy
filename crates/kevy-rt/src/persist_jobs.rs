//! Bodies of the persist worker's teardown/swap jobs — split from
//! `persist_worker.rs` at the 500-LOC line. All run on the worker
//! thread; rationale for keeping this work off the reactor lives in
//! the S5 findings in bench/.

use crate::persist_worker::PersistDone;
use std::path::PathBuf;

/// Hardlink the live log to its graveyard (rename must not free a GB
/// inode's extents inline) and rename the finished image over it.
pub(crate) fn run_swap(tmp: PathBuf, live: PathBuf, trash: Option<PathBuf>) -> PersistDone {
    let linked = match &trash {
        Some(t) => std::fs::hard_link(&live, t).is_ok(),
        None => false,
    };
    PersistDone::SwapImage {
        result: std::fs::rename(&tmp, &live),
        trash: trash.filter(|_| linked),
    }
}

/// Unlink abandoned files and free retained buffers, all off-thread.
pub(crate) fn run_cleanup(paths: Vec<PathBuf>, bufs: Vec<Vec<u8>>) -> PersistDone {
    let mut failed = Vec::new();
    for p in paths {
        if let Err(e) = std::fs::remove_file(&p) {
            failed.push((p, e));
        }
    }
    drop(bufs);
    PersistDone::Cleanup { failed }
}
