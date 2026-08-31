//! One engine per data directory.
//!
//! kevy-persist had no directory-level mutual exclusion: two engines
//! (same process or two) could own one dir, and the second open
//! appended its own `KEVYAOF2` magic into the first one's live AOF —
//! the next replay read that magic as a record header and quarantined
//! it, silently (reported downstream with the bytes: two magics back
//! to back). SQLite, LMDB and redis all refuse that shape outright;
//! now so does kevy. The first open takes `flock(LOCK_EX | LOCK_NB)`
//! on `<dir>/LOCK`; a second gets a plain error naming the directory.
//!
//! Advisory on purpose: the kernel releases the lock with the open
//! file description, so a crash — however hard — never wedges the
//! directory, and there is no stale-pidfile heuristic to get wrong.
//! wasm32 has no second process to race and no kevy-sys to call; the
//! type is a no-op there so the wasm build stands unchanged.

use std::io;
use std::path::Path;

/// Exclusive claim on a persistence directory, held for the life of
/// the engine that opened it. Dropping it releases the claim; the
/// `LOCK` file itself stays behind, empty and inert while unlocked.
///
/// ```
/// let dir = std::env::temp_dir().join(format!("dirlock-type-doc-{}", std::process::id()));
/// let claim = kevy_persist::DirLock::acquire(&dir)?;
/// drop(claim); // released with the engine, or with the process, whichever first
/// # std::fs::remove_dir_all(&dir).ok();
/// # Ok::<(), std::io::Error>(())
/// ```
#[derive(Debug)]
pub struct DirLock {
    #[cfg(not(target_arch = "wasm32"))]
    _file: std::fs::File,
}

impl DirLock {
    /// Claim `dir`, creating it if needed. A directory already claimed
    /// — by this process or another — answers `WouldBlock` with a
    /// message naming the dir, instead of letting two writers
    /// interleave appends into one AOF.
    ///
    /// # Example
    ///
    /// ```
    /// let dir = std::env::temp_dir().join(format!("dirlock-doc-{}", std::process::id()));
    /// let claim = kevy_persist::DirLock::acquire(&dir)?;
    /// // Held: a second engine (any process) is refused, not interleaved.
    /// assert!(kevy_persist::DirLock::acquire(&dir).is_err());
    /// drop(claim);
    /// // Released with the claim: the directory can be owned again.
    /// let again = kevy_persist::DirLock::acquire(&dir)?;
    /// # drop(again);
    /// # std::fs::remove_dir_all(&dir).ok();
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn acquire(dir: &Path) -> io::Result<DirLock> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            std::fs::create_dir_all(dir)?;
            let file = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false) // the file is a lock, never a payload
                .open(crate::layout::lock_path(dir))?;
            use std::os::fd::AsRawFd;
            kevy_sys::flock_try_exclusive(file.as_raw_fd()).map_err(|e| {
                if e.kind() == io::ErrorKind::WouldBlock {
                    io::Error::new(
                        io::ErrorKind::WouldBlock,
                        format!(
                            "kevy data dir {} is already open in another engine \
                             (this process or another); a second writer would \
                             interleave appends into one AOF. Close the first, \
                             or point this open at its own directory.",
                            dir.display()
                        ),
                    )
                } else {
                    e
                }
            })?;
            Ok(DirLock { _file: file })
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = dir;
            Ok(DirLock {})
        }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn second_claim_refused_until_first_drops() {
        let dir = std::env::temp_dir().join(format!(
            "kevy-dirlock-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let first = DirLock::acquire(&dir).expect("first claim");
        let err = DirLock::acquire(&dir).expect_err("second claim must refuse");
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        assert!(err.to_string().contains("already open"), "{err}");
        drop(first);
        let third = DirLock::acquire(&dir).expect("claim after release");
        drop(third);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
