//! Staged appends: records land in a [`StageRing`] — a shared mapping, so
//! a killed process keeps every append that returned — and a drain copies
//! them into the file. The drain runs on every tick and before anything
//! that rewrites, truncates or syncs the file (they all start with
//! `flush_queued`), so the file is complete whenever something else looks.
//!
//! A transaction is never split between the file and the ring: a drain
//! that runs inside one sends the rest of that transaction straight to the
//! file. Replay could not otherwise tell that the ring's half belonged to
//! a transaction whose begin marker the file's half holds.

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use kevy_resp::Argv;

use crate::Fsync;
use crate::aof::Aof;
use crate::replay_walk::{Sink, V2Walk, apply_record};
use crate::stage_recover::{Recovery, recover};
use crate::stage_ring::StageRing;

/// The ring behind a staged log.
#[derive(Debug)]
pub(crate) struct Stage {
    ring: StageRing,
    path: PathBuf,
    /// A drain ran inside the open transaction: its remaining records go
    /// straight to the file.
    overflow: bool,
}

/// What [`Aof::open_stage`] found in the ring the last process left.
///
/// ```
/// let found = kevy_persist::StageOpen::default();
/// assert_eq!((found.recovered, found.discarded, found.torn), (0, None, false));
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct StageOpen {
    /// Records the ring held that the log did not, now replayed and
    /// appended to the log.
    pub recovered: u64,
    /// Why the ring was set aside instead, when it was.
    pub discarded: Option<&'static str>,
    /// The ring's records stopped at one that failed its checksum.
    pub torn: bool,
}

impl Aof {
    /// Stage this log's appends in a ring of `cap` bytes at `path`, after
    /// settling what the ring left by the last process owes: records it
    /// committed that never reached the log are handed to `apply` (a
    /// transaction only whole, once its commit marker is seen) and appended
    /// to the log. Call once, right after the log itself was replayed.
    ///
    /// A v1 log, or one in queued mode, settles the ring instead of staging
    /// ([`Self::settle_stage`]); under `Always` appends bypass the ring.
    ///
    /// ```
    /// use kevy_persist::{Aof, Argv, Fsync};
    ///
    /// let dir = std::env::temp_dir().join(format!("stage-doc-{}", std::process::id()));
    /// std::fs::create_dir_all(&dir)?;
    /// let mut log = Aof::open(&dir.join("doc.aof"), Fsync::EverySec)?;
    /// let found = log.open_stage(&dir.join("doc.stage"), 64 * 1024, |_| {})?;
    /// assert_eq!(found.recovered, 0, "a fresh ring owes nothing");
    /// log.append(&Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]))?;
    /// assert_eq!(log.stage_path(), Some(dir.join("doc.stage").as_path()));
    /// # drop(log);
    /// # std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn open_stage(
        &mut self,
        path: &Path,
        cap: u64,
        apply: impl FnMut(&mut Argv),
    ) -> io::Result<StageOpen> {
        if self.format != crate::AofFormat::V2 || self.queue.is_some() {
            return self.settle_stage(path, apply);
        }
        let found = self.recover_stage(path, apply)?;
        let (ino, len) = self.identity()?;
        let ring = StageRing::create(path, cap, ino, len)?;
        self.stage = Some(Stage { ring, path: path.to_path_buf(), overflow: false });
        Ok(found)
    }

    /// Settle what a ring at `path` owes this log, as [`Self::open_stage`]
    /// does, then remove the ring: for a log that no longer stages, or whose
    /// shard layout is about to change.
    ///
    /// ```
    /// use kevy_persist::{Aof, Fsync};
    ///
    /// let dir = std::env::temp_dir().join(format!("settle-doc-{}", std::process::id()));
    /// std::fs::create_dir_all(&dir)?;
    /// let ring = dir.join("doc.stage");
    /// let mut log = Aof::open(&dir.join("doc.aof"), Fsync::EverySec)?;
    /// log.open_stage(&ring, 64 * 1024, |_| {})?;
    /// drop(log);
    /// let mut log = Aof::open(&dir.join("doc.aof"), Fsync::EverySec)?;
    /// log.settle_stage(&ring, |_| {})?;
    /// assert!(!ring.exists());
    /// # drop(log);
    /// # std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn settle_stage(
        &mut self,
        path: &Path,
        apply: impl FnMut(&mut Argv),
    ) -> io::Result<StageOpen> {
        let found = self.recover_stage(path, apply)?;
        match std::fs::remove_file(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(found),
        }
    }

    fn recover_stage(
        &mut self,
        path: &Path,
        mut apply: impl FnMut(&mut Argv),
    ) -> io::Result<StageOpen> {
        let mut found = StageOpen::default();
        self.file.flush()?;
        let (ino, len) = self.identity()?;
        if let Some((ring, head)) = StageRing::open_existing(path)? {
            let tail = self.tail_after(head.aof_len, len, ring.cap())?;
            match recover(&ring, head, ino, len, &tail) {
                Recovery::Discard(why) => found.discarded = Some(why),
                Recovery::Replay { records, torn } => {
                    found.torn = torn;
                    found.recovered = self.adopt(&records, &mut apply)?;
                }
            }
        }
        Ok(found)
    }

    /// Stage one record of `len` bytes, written by `fill`. `false` when it
    /// must go to the file instead: no ring, `Always`, a transaction that
    /// already overflowed, or a record larger than the ring.
    pub(crate) fn stage_record(
        &mut self,
        len: usize,
        fill: impl Fn(&mut [u8]),
    ) -> io::Result<bool> {
        if matches!(self.fsync, Fsync::Always) || self.stage.as_ref().is_none_or(|s| s.overflow) {
            return Ok(false);
        }
        if self.stage.as_mut().is_some_and(|s| s.ring.push(len, &fill)) {
            return Ok(true);
        }
        self.drain_stage()?;
        let stage = self.stage.as_mut().expect("checked above");
        Ok(!stage.overflow && stage.ring.push(len, &fill))
    }

    /// Copy every staged record into the file (into the kernel, not
    /// synced). Inside a transaction, the rest of it then bypasses the ring.
    pub(crate) fn drain_stage(&mut self) -> io::Result<()> {
        let Some(stage) = &mut self.stage else {
            return Ok(());
        };
        self.file.flush()?;
        let file = self.file.get_mut();
        let mut failed = None;
        let end = stage.ring.for_each_pending(|run| {
            if failed.is_none() {
                failed = file.write_all(run).err();
            }
        });
        if let Some(e) = failed {
            return Err(e);
        }
        let len = file.metadata()?.len();
        stage.ring.mark_drained(end, len);
        if self.in_txn {
            stage.overflow = true;
        }
        Ok(())
    }

    /// The transaction closed: its successors may stage again.
    pub(crate) fn stage_txn_closed(&mut self) {
        if let Some(stage) = &mut self.stage {
            stage.overflow = false;
        }
    }

    /// A record went to the file instead of the ring. It returned, so a
    /// killed process must keep it, as it keeps a staged one: push it into
    /// the kernel. The file then outgrew what the ring's header says, so the
    /// header follows — the ring is empty here, everything before went out
    /// with the drain that made the room check fail or the overflow start.
    pub(crate) fn stage_bypassed(&mut self) -> io::Result<()> {
        if self.stage.is_some() {
            self.rebase_stage()?;
        }
        Ok(())
    }

    /// The file was replaced or emptied under the ring: everything the ring
    /// committed is reflected in the new file, so it now continues that one.
    pub(crate) fn rebase_stage(&mut self) -> io::Result<()> {
        if self.stage.is_none() {
            return Ok(());
        }
        self.file.flush()?;
        let (ino, len) = self.identity()?;
        if let Some(stage) = &mut self.stage {
            stage.ring.rebase(ino, len);
        }
        Ok(())
    }

    /// Where the ring lives, when this log stages.
    ///
    /// ```
    /// let path = std::env::temp_dir().join(format!("stage-path-doc-{}.aof", std::process::id()));
    /// let log = kevy_persist::Aof::open(&path, kevy_persist::Fsync::No)?;
    /// assert_eq!(log.stage_path(), None, "a log stages only once asked to");
    /// # drop(log);
    /// # std::fs::remove_file(&path)?;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn stage_path(&self) -> Option<&Path> {
        self.stage.as_ref().map(|s| s.path.as_path())
    }

    fn identity(&self) -> io::Result<(u64, u64)> {
        let meta = self.file.get_ref().metadata()?;
        Ok((meta.ino(), meta.len()))
    }

    /// The log's bytes in `[from, to)`, or none when that span is not one a
    /// ring of `cap` bytes could account for.
    fn tail_after(&self, from: u64, to: u64, cap: u64) -> io::Result<Vec<u8>> {
        if to < from || to - from > cap {
            return Ok(Vec::new());
        }
        let mut f = std::fs::File::open(&self.path)?;
        f.seek(SeekFrom::Start(from))?;
        let mut tail = vec![0u8; (to - from) as usize];
        f.read_exact(&mut tail)?;
        Ok(tail)
    }

    /// Replay recovered records through `apply` and append them to the log,
    /// up to the last record outside a transaction: a trailing transaction
    /// the ring never saw committed is dropped, and appending its begin
    /// marker would make every later record look like part of it.
    fn adopt(&mut self, records: &[Vec<u8>], apply: &mut impl FnMut(&mut Argv)) -> io::Result<u64> {
        let mut walk = V2Walk::at(0);
        let mut args = Argv::default();
        let mut sink = Some(Sink::InPlace(apply));
        let mut settled = 0;
        for (i, rec) in records.iter().enumerate() {
            let payload = &rec[crate::record::RECORD_HEADER..];
            if !apply_record(payload, payload.len() as u32, &mut args, &mut sink, &mut walk) {
                break;
            }
            if walk.txn.is_none() {
                settled = i + 1;
            }
        }
        if settled == 0 {
            return Ok(0);
        }
        let file = self.file.get_mut();
        for rec in &records[..settled] {
            file.write_all(rec)?;
        }
        file.sync_data()?;
        self.size_bytes += records[..settled].iter().map(|r| r.len() as u64).sum::<u64>();
        Ok(settled as u64)
    }
}
