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
pub fn unique_dir(label: &str) -> PathBuf {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!("kevy-{label}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("create temp dir");
    p
}

/// A unique directory under the system temp dir, removed when dropped.
///
/// Dropped on unwind too, so a failing test does not leave litter behind for the
/// next one to trip over.
#[derive(Debug)]
pub struct TmpDir(PathBuf);

impl TmpDir {
    /// `label` shows up in the path, so a directory that somehow survives says
    /// which test left it.
    pub fn new(label: &str) -> Self {
        Self(unique_dir(label))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for TmpDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

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
            let handles: Vec<_> = (0..32)
                .map(|_| s.spawn(|| TmpDir::new("race").path().to_path_buf()))
                .collect();
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
}
