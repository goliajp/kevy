//! The timed half of the fsync policy: what a periodic tick does for
//! `EverySec` and `No`. The `EverySec` fsync is handed back to the
//! caller as a [`PendingSync`], so a caller that ticks under its own
//! lock can release the lock before the disk flush runs.

use std::fs::File;
use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::aof::Aof;
use crate::Fsync;

/// An `EverySec` fsync that [`Aof::tick`] started but did not run.
///
/// It holds its own handle to the log file, so it does not borrow the
/// [`Aof`]: appends may continue while it runs. It covers every record
/// appended before the `tick` that returned it.
///
/// ```
/// use kevy_persist::{Aof, Argv, Fsync};
///
/// # fn main() -> std::io::Result<()> {
/// let path = std::env::temp_dir().join(format!("kevy-pending-sync-{}.aof", std::process::id()));
/// # let _ = std::fs::remove_file(&path);
/// let mut aof = Aof::open(&path, Fsync::EverySec)?;
/// aof.append(&Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]))?;
/// // `Some` only once the one-second window has elapsed
/// if let Some(sync) = aof.tick()? {
///     sync.run()?;
/// }
/// # drop(aof);
/// # std::fs::remove_file(&path)?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
#[must_use = "the fsync only happens when `run` is called"]
pub struct PendingSync {
    file: File,
    /// The mapped chunks to `msync` first, for a log that maps its appends.
    maps: Vec<crate::aof::MapHandle>,
    generation: u64,
    confirmed: Arc<AtomicU64>,
}

impl PendingSync {
    /// Run the fsync (`fdatasync`; `F_FULLFSYNC` on Apple platforms).
    /// On failure the log stays unconfirmed: the next [`Aof::tick`]
    /// retries without waiting for the window, and [`Aof::sync_now`]
    /// does not skip its own sync.
    ///
    /// ```
    /// use kevy_persist::{Aof, Fsync};
    ///
    /// # fn main() -> std::io::Result<()> {
    /// let path = std::env::temp_dir().join(format!("kevy-pending-run-{}.aof", std::process::id()));
    /// # let _ = std::fs::remove_file(&path);
    /// let mut aof = Aof::open(&path, Fsync::EverySec)?;
    /// let pending = aof.tick()?; // `None`: nothing appended yet
    /// if let Some(sync) = pending {
    ///     sync.run()?;
    /// }
    /// # drop(aof);
    /// # std::fs::remove_file(&path)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn run(self) -> io::Result<()> {
        crate::aof::sync_handles(&self.maps)?;
        self.file.sync_data()?;
        self.confirmed.fetch_max(self.generation, Ordering::Release);
        Ok(())
    }
}

impl Aof {
    /// The per-tick upkeep of the fsync policy. Call it once per tick.
    ///
    /// - `No` and `EverySec`: writes the user-space buffer into the
    ///   kernel (no fsync), so a killed process keeps everything appended
    ///   before the tick.
    /// - `EverySec`, in addition: once a second has passed since the last
    ///   sync started, returns the fsync as a [`PendingSync`] for the
    ///   caller to run, typically after releasing whatever lock guards
    ///   this log.
    /// - `Always`: nothing to do; every append already synced.
    ///
    /// ```
    /// use kevy_persist::{Aof, Argv, Fsync};
    ///
    /// # fn main() -> std::io::Result<()> {
    /// let path = std::env::temp_dir().join(format!("kevy-aof-tick-{}.aof", std::process::id()));
    /// # let _ = std::fs::remove_file(&path);
    /// let mut aof = Aof::open(&path, Fsync::No)?;
    /// let before = std::fs::metadata(&path)?.len();
    /// aof.append(&Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]))?;
    /// assert!(aof.tick()?.is_none());
    /// // the record left the process: it is in the file, unsynced
    /// assert!(std::fs::metadata(&path)?.len() > before);
    /// # drop(aof);
    /// # std::fs::remove_file(&path)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn tick(&mut self) -> io::Result<Option<PendingSync>> {
        if matches!(self.fsync, Fsync::Always) {
            return Ok(None);
        }
        self.drain_stage()?;
        self.file.flush()?;
        match self.fsync {
            Fsync::EverySec => self.start_everysec_sync(),
            Fsync::No | Fsync::Always => Ok(None),
        }
    }

    /// Write the buffer into the kernel (`No`, `EverySec`), and fsync
    /// if the `EverySec` window has elapsed. Call once per loop tick. The
    /// fsync runs inline — [`Self::tick`] is the variant that hands it
    /// back.
    pub fn maybe_sync(&mut self) -> io::Result<()> {
        if let Some(sync) = self.tick()? {
            sync.run()?;
        }
        Ok(())
    }

    // `tick` has just written every buffered record into the kernel, and
    // all writes go through that one buffer in append order, so the file
    // in the kernel is always a prefix of the log: the fsync on the handle
    // taken here covers everything appended before this tick and can never
    // cover a record whose predecessors are still in user space
    fn start_everysec_sync(&mut self) -> io::Result<Option<PendingSync>> {
        let retry = self.sync_unconfirmed();
        let due = self.dirty && self.last_sync.elapsed() >= Duration::from_secs(1);
        if !(due || retry) {
            return Ok(None);
        }
        self.file.get_ref().try_clone().map(|file| {
            self.dirty = false;
            self.last_sync = Instant::now();
            self.sync_started += 1;
            Some(PendingSync {
                file,
                maps: self.map_handles(),
                generation: self.sync_started,
                confirmed: Arc::clone(&self.sync_confirmed),
            })
        })
    }

    /// A sync was started whose fsync has not been confirmed — still
    /// running elsewhere, or failed. `dirty` is already clear for those
    /// records, so barriers must not trust it alone.
    pub(crate) fn sync_unconfirmed(&self) -> bool {
        self.sync_confirmed.load(Ordering::Acquire) < self.sync_started
    }

    /// An inline sync of the live file covered everything started so far.
    pub(crate) fn confirm_started_syncs(&self) {
        self.sync_confirmed.fetch_max(self.sync_started, Ordering::Release);
    }
}
