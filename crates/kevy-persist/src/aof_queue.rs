//! The queued-append half of [`Aof`] (RFC v3-aof-offload S1) — the
//! driver-facing drain API plus the honest synchronous fallback the
//! structural entry points call. Split from `aof.rs` at the 500-LOC
//! line; the mode's contract lives on the `queue` field's doc.

use std::io::{self, Write};
use std::time::{Duration, Instant};

use super::aof::Aof;
use crate::Fsync;

impl Aof {
    /// Switch this log to queued-append mode (see the `queue` field
    /// doc). Idempotent. The uring reactor calls this once at setup;
    /// nothing else should.
    pub fn enable_queued_appends(&mut self) {
        if self.queue.is_none() {
            self.queue = Some(Vec::new());
        }
    }

    /// Queued mode's drain: the accumulated record bytes since the last
    /// take (`None` when empty or not in queued mode), paired with the
    /// file offset the chunk must be written at. Offsets advance per
    /// take, so overlapping in-flight chunks stay disjoint.
    pub fn take_pending(&mut self) -> Option<(u64, Vec<u8>)> {
        let q = self.queue.as_mut()?;
        if q.is_empty() {
            return None;
        }
        let chunk = std::mem::take(q);
        let at = self.queued_offset;
        self.queued_offset += chunk.len() as u64;
        Some((at, chunk))
    }

    /// S2 reply-gate watermark: how many records have ever been queued.
    /// Monotone for the life of this log (never resets on rewrite), so
    /// the driver can compare it against its fsync-proven durable
    /// watermark without wedging held replies across a file swap.
    #[must_use]
    pub fn queued_watermark(&self) -> u64 {
        self.queued_seq
    }

    /// Whether queued-append (offload) mode is on — the discriminant
    /// for protocol choices that are only safe when the driver can
    /// hold the append stream (e.g. the off-thread swap).
    #[must_use]
    pub fn queued_mode(&self) -> bool {
        self.queue.is_some()
    }

    /// Nothing waiting in the queue (always true outside queued mode).
    /// The offload driver's restructure gate checks this alongside its
    /// in-flight set, so a rewrite's `begin` never has bytes to flush.
    #[must_use]
    pub fn queued_is_empty(&self) -> bool {
        self.queue.as_ref().is_none_or(Vec::is_empty)
    }

    /// The fd queued chunks are written to (the driver's SQE target).
    /// Unix-only — the driver is the io_uring reactor (Linux); wasm and
    /// other fd-less targets never enable queued appends.
    #[cfg(unix)]
    #[must_use]
    pub fn queued_fd(&self) -> Option<std::os::fd::RawFd> {
        use std::os::fd::AsRawFd;
        self.queue.as_ref().map(|_| self.file.get_ref().as_raw_fd())
    }

    /// A cloned handle to the live log file (the writer-thread lane's
    /// target — S3). The clone shares the O_APPEND file description,
    /// so the thread's sequential `write_all`s land exactly where the
    /// synchronous path's would; after a rewrite swap the caller must
    /// hand the lane a FRESH clone (the old one points at the
    /// renamed-away inode). `None` outside queued mode.
    pub fn queued_file_clone(&self) -> Option<std::io::Result<std::fs::File>> {
        self.queue.as_ref().map(|_| self.file.get_ref().try_clone())
    }

    /// Honest fallback for the structural entry points (truncate,
    /// rewrite finish, Always upgrade): synchronously write whatever is
    /// still queued HERE so the file is self-consistent before the
    /// operation. In-flight chunks already taken are the driver's to
    /// order — the contract on the `queue` field.
    pub(crate) fn flush_queued(&mut self) -> io::Result<()> {
        let Some(q) = &mut self.queue else {
            return Ok(());
        };
        if q.is_empty() {
            return Ok(());
        }
        let chunk = std::mem::take(q);
        self.queued_offset += chunk.len() as u64;
        self.file.write_all(&chunk)?;
        Ok(())
    }

    /// Durability barrier: flush + `fdatasync` NOW, regardless
    /// of the fsync policy. On return, every append made so far is on
    /// stable storage. Lets an `EverySec` deployment make individual
    /// critical writes durable-on-ack (Postgres
    /// `synchronous_commit`-per-transaction genre) without paying
    /// `Always` on every op. No-op cost when nothing is dirty.
    pub fn sync_now(&mut self) -> io::Result<()> {
        self.flush_queued()?;
        if self.dirty {
            self.file.flush()?;
            self.file.get_ref().sync_data()?;
            self.dirty = false;
            self.last_sync = Instant::now();
        }
        Ok(())
    }

    /// Flush+fsync if the `EverySec` window has elapsed. Call once per loop tick.
    pub fn maybe_sync(&mut self) -> io::Result<()> {
        if matches!(self.fsync, Fsync::EverySec)
            && self.dirty
            && self.last_sync.elapsed() >= Duration::from_secs(1)
        {
            self.file.flush()?;
            self.file.get_ref().sync_data()?;
            self.dirty = false;
            self.last_sync = Instant::now();
        }
        Ok(())
    }
}
