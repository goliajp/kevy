//! A temporary directory that is actually unique, and cleans up after itself.
//!
//! Nine files in this workspace had invented their own, and five different ways
//! of trying to make the name unique:
//!
//! ```text
//! kevy-persist/feed_meta.rs    process::id() + Instant::now()
//! kevy-persist/shards_meta.rs  process::id() + process::id()
//! kevy-persist/reshard.rs      SystemTime
//! kevy-cli/backup.rs           nanos
//! kevy-uring/ring_tests.rs     process::id()
//! …
//! ```
//!
//! None of them is unique. `process::id()` is the SAME for every test in a
//! binary — cargo runs them as threads, not processes — so two tests in the same
//! file get the same directory and stamp on each other's files. A clock is
//! unique only until two threads read it inside the same tick, which under
//! parallel test execution is exactly what happens. That is a flake: green on a
//! quiet machine, red under load, and it looks like the code is broken rather
//! than the fixture.
//!
//! A process id and a monotonic counter, together, cannot collide: the counter
//! separates threads within a process and the pid separates processes. That is
//! the whole trick, and it is why this is one crate instead of nine copies.
//!
//! ```
//! let dir = kevy_tmpdir::TmpDir::new("readme");
//! std::fs::write(dir.path().join("data"), b"x")?;
//! let path = dir.path().to_path_buf();
//! drop(dir);
//! assert!(!path.exists(), "gone with the guard");
//! # Ok::<(), std::io::Error>(())
//! ```

// Best-effort removal, on paths where the file is being abandoned.
// A file that will not delete is a stray the next sweep collects,
// and refusing here would abandon the rest of the cleanup.
#![expect(clippy::let_underscore_must_use, reason = "removing what is already meant to be gone")]
#![warn(missing_docs)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// A unique directory as a bare path: created, PRE-CLEARED, and owned by the
/// caller — for call sites that already carry their own cleanup, and for the
/// one production path (kevy-cli's embed scratch) where the directory must
/// outlive the function that made it.
///
/// The pre-clear matters more than it looks. The old pid-only scratch dir was
/// never cleared, so a recycled pid inherited the PREVIOUS run's data files —
/// `create_dir_all` on an existing directory succeeds silently, and the loader
/// then read a mix of stale and fresh dumps as though they were one dataset.
///
/// # Panics
///
/// When the directory cannot be created: a caller with nowhere to put its
/// files has nothing to do next.
///
/// # Examples
///
/// Two calls never collide, and the second call for a label does not
/// inherit what the first one left:
///
/// ```
/// let a = kevy_tmpdir::unique_dir("doc");
/// let b = kevy_tmpdir::unique_dir("doc");
/// assert_ne!(a, b);
/// assert!(a.is_dir() && b.is_dir());
///
/// std::fs::write(a.join("stale"), b"old").unwrap();
/// let again = kevy_tmpdir::unique_dir("doc");
/// assert!(!again.join("stale").exists());
///
/// for d in [a, b, again] { std::fs::remove_dir_all(d).unwrap(); }
/// ```
pub fn unique_dir(label: &str) -> PathBuf {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!("kevy-{label}-{}-{n}", std::process::id()));
    // A leftover from a previous process that reused this pid: it is being
    // replaced, so its removal failing is not this call's problem.
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p)
        .expect("a caller with nowhere to put its files has nothing to do next");
    p
}

/// A unique directory under the system temp dir, removed when dropped.
///
/// Dropped on unwind too, so a failing test does not leave litter behind for the
/// next one to trip over.
#[derive(Debug)]
/// # Examples
///
/// ```
/// use std::path::Path;
/// let dir = kevy_tmpdir::TmpDir::new("doctest");
/// let kept: std::path::PathBuf = dir.path().to_path_buf();
/// assert!(kept.is_dir());
/// drop(dir);
/// assert!(!kept.exists(), "the guard deletes the directory it made");
/// ```
pub struct TmpDir(PathBuf);

impl TmpDir {
    /// `label` shows up in the path, so a directory that somehow survives says
    /// which test left it.
    ///
    /// # Panics
    ///
    /// When the directory cannot be created, as [`unique_dir`].
    ///
    /// # Examples
    ///
    /// ```
    /// let dir = kevy_tmpdir::TmpDir::new("who-left-this");
    /// let name = dir.path().file_name().unwrap().to_string_lossy().into_owned();
    /// assert!(name.contains("who-left-this"), "{name} does not name its owner");
    /// ```
    pub fn new(label: &str) -> Self {
        Self(unique_dir(label))
    }

    /// The directory itself. Borrowed, not cloned, so the path cannot
    /// outlive the guard that deletes it — a `PathBuf` handed out here
    /// would still name the directory after `Drop` removed it.
    ///
    /// # Examples
    ///
    /// ```
    /// let dir = kevy_tmpdir::TmpDir::new("write-something");
    /// std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
    /// assert_eq!(std::fs::read(dir.path().join("a.txt")).unwrap(), b"hello");
    /// ```
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Remove the directory now and say whether that worked — what `Drop`
    /// does, for a caller that cares about the answer. Dropping the guard
    /// cannot report a failure, and removing a large tree blocks for as
    /// long as it takes.
    ///
    /// # Examples
    ///
    /// ```
    /// let dir = kevy_tmpdir::TmpDir::new("close-me");
    /// let path = dir.path().to_path_buf();
    /// dir.close()?;
    /// assert!(!path.exists());
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn close(mut self) -> std::io::Result<()> {
        // an empty path tells `Drop` there is nothing left to remove
        let path = std::mem::take(&mut self.0);
        std::fs::remove_dir_all(path)
    }
}

impl AsRef<Path> for TmpDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        // Drop cannot report, and a temp directory that outlives its process
        // is the OS's to reclaim.
        if !self.0.as_os_str().is_empty() {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

// Send and Sync are part of the public contract: a change that loses
// either fails to compile here rather than in a caller.
const _: () = {
    const fn send_sync<T: Send + Sync>() {}
    send_sync::<TmpDir>();
};

#[cfg(test)]
mod tests {
    use super::TmpDir;
    use std::collections::HashSet;

    #[test]
    fn two_dirs_in_one_process_are_different() {
        // The bug, stated: process::id() is the same for both of these.
        let a = TmpDir::new("x");
        let b = TmpDir::new("x");
        assert_ne!(a.path(), b.path());
    }

    #[test]
    fn threads_racing_for_a_name_all_get_their_own() {
        let dirs: Vec<_> = std::thread::scope(|s| {
            let handles: Vec<_> =
                (0..32).map(|_| s.spawn(|| TmpDir::new("race").path().to_path_buf())).collect();
            handles.into_iter().map(|h| h.join().expect("thread")).collect()
        });
        let uniq: HashSet<_> = dirs.iter().collect();
        assert_eq!(uniq.len(), 32, "two threads got the same directory");
    }

    #[test]
    fn the_directory_exists_and_then_does_not() {
        let p = {
            let d = TmpDir::new("drop");
            assert!(d.path().is_dir());
            d.path().to_path_buf()
        };
        assert!(!p.exists(), "TmpDir did not clean up after itself");
    }

    #[test]
    fn close_removes_the_tree_and_reports_a_failure() {
        let d = TmpDir::new("close");
        let p = d.path().to_path_buf();
        std::fs::create_dir(p.join("sub")).unwrap();
        std::fs::write(p.join("sub/f"), b"x").unwrap();
        d.close().unwrap();
        assert!(!p.exists(), "close left the directory behind");

        let gone = TmpDir::new("close-gone");
        std::fs::remove_dir_all(gone.path()).unwrap();
        let err = gone.close().unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }
}
