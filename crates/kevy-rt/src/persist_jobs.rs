//! Bodies of the persist worker's teardown/swap jobs — split from
//! `persist_worker.rs` at the 500-LOC line. All run on the worker
//! thread; rationale for keeping this work off the reactor lives in
//! the S5 findings in bench/.

use crate::persist_worker::{PersistDone, PersistJob};
use std::path::PathBuf;

/// Append + fsync the residual tee tail into the image (trickle
/// workloads never drain the tee empty, and this write belongs off the
/// reactor with the rest), hardlink the live log to its graveyard
/// (rename must not free a GB inode's extents inline), and rename the
/// finished image over it.
pub(crate) fn run_swap(
    tmp: PathBuf,
    live: PathBuf,
    trash: Option<PathBuf>,
    tail: Vec<u8>,
) -> PersistDone {
    if !tail.is_empty() {
        let appended = std::fs::OpenOptions::new()
            .append(true)
            .open(&tmp)
            .and_then(|mut f| {
                std::io::Write::write_all(&mut f, &tail)?;
                f.sync_all()
            });
        if let Err(e) = appended {
            // Image incomplete without the tail — abort the swap; the
            // live log carried every write through the normal path.
            return PersistDone::SwapImage { result: Err(e), trash: None };
        }
    }
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
            crate::persist_worker::drop_file_cache(&f);
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

pub(crate) fn run_job(job: PersistJob) -> PersistDone {
    match job {
        PersistJob::Save {
            view,
            snap_path,
            aof_reset,
            cursor,
        } => PersistDone::Save {
            result: crate::persist_worker::write_snapshot_tmp_with_cursor(&view, &snap_path, cursor),
            snap_path,
            aof_reset,
        },
        PersistJob::Rewrite { view, tmp } => PersistDone::Rewrite {
            // dump_aof drop-behinds its own cache and sync_all()s.
            result: kevy_persist::dump_aof(&tmp, &view).map(|(keys, _bytes)| keys),
            tmp,
        },
        PersistJob::SwapImage { tmp, live, trash, tail } => {
            run_swap(tmp, live, trash, tail)
        }
        PersistJob::Cleanup { paths, bufs } => run_cleanup(paths, bufs),
        PersistJob::TeeAppend { tmp, bytes } => run_tee_append(tmp, bytes),
    }
}
