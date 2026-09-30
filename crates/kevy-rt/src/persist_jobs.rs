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
        let appended = std::fs::OpenOptions::new().append(true).open(&tmp).and_then(|mut f| {
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
    // Make the rename itself crash-durable: the image's DATA is
    // sync_all'd (snapshot dump, every tee generation, the tail
    // above), but the new name linkage is directory metadata. The
    // Always reply gate treats a completed swap as proof of
    // durability (uring_aof_mark_all_durable), so the directory
    // entry must survive power loss too. A dir-sync failure does NOT
    // un-commit the swap — the rename is the commit point; report it
    // loudly (same contract as an fsync failure) and carry on.
    let result = std::fs::rename(&tmp, &live);
    if result.is_ok()
        && let Some(d) = live.parent()
        && let Err(e) = std::fs::File::open(d).and_then(|f| f.sync_all())
    {
        eprintln!("kevy: aof swap directory sync failed: {e}");
    }
    PersistDone::SwapImage { result, trash: trash.filter(|_| linked) }
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
    PersistDone::TeeAppend { result, tmp, buf: bytes }
}

pub(crate) fn run_job(job: PersistJob) -> PersistDone {
    match job {
        PersistJob::Save { view, snap_path, aof_reset, cursor, aux } => PersistDone::Save {
            result: crate::persist_worker::write_snapshot_tmp_with_cursor(
                &kevy_persist::WithAux::new(&view, aux.as_ref()),
                &snap_path,
                cursor,
            ),
            snap_path,
            aof_reset,
        },
        PersistJob::Rewrite { view, tmp, aux } => PersistDone::Rewrite {
            // dump_aof drop-behinds its own cache and sync_all()s.
            result: kevy_persist::dump_aof(&tmp, &kevy_persist::WithAux::new(&view, aux.as_ref()))
                .map(|stats| stats.keys),
            tmp,
        },
        PersistJob::SwapImage { tmp, live, trash, tail } => run_swap(tmp, live, trash, tail),
        PersistJob::Cleanup { paths, bufs } => run_cleanup(paths, bufs),
        PersistJob::TeeAppend { tmp, bytes } => run_tee_append(tmp, bytes),
    }
}

/// Append `frame` to a shard's log and sync it: the `record` callback of
/// [`crate::Commands::on_restored`].
pub(crate) fn record_durably(
    aof: &mut Option<kevy_persist::Aof>,
    shard: usize,
    frame: &kevy_resp::Argv,
) -> bool {
    let Some(aof) = aof.as_mut() else { return false };
    match aof.append(frame).and_then(|()| aof.sync_now()) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("kevy: shard {shard} could not record a restored frame: {e}");
            false
        }
    }
}

/// Load a shard's snapshot file at boot, returning the frame it kept
/// beside the keyspace.
pub(crate) fn load_snapshot_file(
    store: &mut kevy_store::Store,
    path: &std::path::Path,
) -> std::io::Result<Option<kevy_resp::Argv>> {
    let file = std::io::BufReader::new(std::fs::File::open(path)?);
    kevy_persist::load_snapshot_with_aux(store, file, |_| true)
}

/// Serialize a replica's snapshot off the reactor. On an error the sender
/// drops, the receiver reads Disconnected, and `pump_snapshot_chunks`
/// closes the connection.
#[expect(
    clippy::let_underscore_must_use,
    reason = "a send fails only once the replica is gone, which its connection reports"
)]
pub(crate) fn spawn_serializer(
    view: kevy_store::SnapshotView,
    aux: Option<kevy_resp::Argv>,
    replica_id: &str,
) -> std::sync::mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name(format!("kevy-snapshot-{replica_id}"))
        .spawn(move || {
            let mut buf = Vec::new();
            let src = kevy_persist::WithAux::new(&view, aux.as_ref());
            if kevy_persist::write_snapshot_to(&src, &mut buf).is_ok() {
                let _ = tx.send(buf);
            }
        })
        .expect("spawn snapshot serializer thread");
    rx
}

#[cfg(test)]
mod tests {
    use super::{load_snapshot_file, record_durably};

    #[test]
    fn a_shard_without_a_log_records_nothing() {
        let frame = kevy_resp::Argv::from(vec![b"XINTERNAL.EXAMPLE".to_vec()]);
        assert!(!record_durably(&mut None, 0, &frame));
    }

    #[test]
    fn a_missing_snapshot_file_is_an_error() {
        let missing = std::env::temp_dir()
            .join(format!("kevy-rt-no-such-dir-{}", std::process::id()))
            .join("dump-0.rdb");
        let mut store = kevy_store::Store::new();
        let err = load_snapshot_file(&mut store, &missing).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }
}
