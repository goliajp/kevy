//! File-backed [`ElectorPersist`] — the kevy-server side of the
//! election-hardening contract. Stores the elector's
//! `(epoch, votedFor)` pair in `<data_dir>/elect.meta`:
//!
//! ```text
//! epoch <n>
//! voted <id|->
//! ```
//!
//! Write discipline follows the `kevy_persist::feed_meta` convention:
//! write to a `.tmp` sibling, `sync_all`, then `rename` into place —
//! a crash mid-save leaves either the old pair or the new pair, never
//! a torn file. `save` is synchronous (fsync before returning), which
//! is exactly what the elector's "durable before the ACCEPT leaves"
//! rule requires.

use std::io::Write;
use std::path::{Path, PathBuf};

use kevy_elect::ElectorPersist;

/// `(epoch, votedFor)` store at `<data_dir>/elect.meta`.
pub(crate) struct FileElectorPersist {
    path: PathBuf,
    tmp: PathBuf,
}

impl FileElectorPersist {
    /// Point the store at `<dir>/elect.meta`. No I/O happens here —
    /// the first `save` creates the file.
    pub(crate) fn new(dir: &Path) -> Self {
        Self { path: dir.join("elect.meta"), tmp: dir.join("elect.meta.tmp") }
    }

    fn write_atomic(&self, body: &[u8]) -> std::io::Result<()> {
        {
            let mut f = std::fs::File::create(&self.tmp)?;
            f.write_all(body)?;
            f.sync_all()?;
        }
        std::fs::rename(&self.tmp, &self.path)
    }
}

impl ElectorPersist for FileElectorPersist {
    fn save(&self, epoch: u64, voted_for: Option<&str>) {
        let body = format!("epoch {epoch}\nvoted {}\n", voted_for.unwrap_or("-"));
        if let Err(e) = self.write_atomic(body.as_bytes()) {
            // Same failure posture as the rest of the elect
            // integration: log loudly, keep serving — the data plane
            // must not die because the meta file is unwritable. The
            // durability guarantee is degraded until the operator
            // fixes the disk.
            eprintln!("kevy: elect.meta save failed at {}: {e}", self.path.display());
        }
    }

    fn load(&self) -> (u64, Option<String>) {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return (0, None); // fresh node / no file yet
        };
        let mut epoch = 0u64;
        let mut voted = None;
        for line in text.lines() {
            if let Some(v) = line.strip_prefix("epoch ") {
                epoch = v.trim().parse().unwrap_or(0);
            } else if let Some(v) = line.strip_prefix("voted ") {
                let v = v.trim();
                if v != "-" && !v.is_empty() {
                    voted = Some(v.to_string());
                }
            }
        }
        (epoch, voted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        kevy_tmpdir::unique_dir(&format!("elect-{tag}"))
    }

    #[test]
    fn load_missing_file_is_fresh() {
        let d = tmp_dir("fresh");
        let p = FileElectorPersist::new(&d);
        assert_eq!(p.load(), (0, None));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn save_load_round_trip_with_vote() {
        let d = tmp_dir("vote");
        let p = FileElectorPersist::new(&d);
        p.save(7, Some("node-b"));
        assert_eq!(p.load(), (7, Some("node-b".to_string())));
        // On-disk format is the documented two-liner.
        let text = std::fs::read_to_string(d.join("elect.meta")).unwrap();
        assert_eq!(text, "epoch 7\nvoted node-b\n");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn save_load_round_trip_without_vote() {
        let d = tmp_dir("novote");
        let p = FileElectorPersist::new(&d);
        p.save(3, None);
        assert_eq!(p.load(), (3, None));
        assert_eq!(std::fs::read_to_string(d.join("elect.meta")).unwrap(), "epoch 3\nvoted -\n");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn save_overwrites_previous_pair() {
        let d = tmp_dir("overwrite");
        let p = FileElectorPersist::new(&d);
        p.save(2, Some("a"));
        p.save(5, None);
        assert_eq!(p.load(), (5, None));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn corrupt_file_reads_as_fresh() {
        let d = tmp_dir("corrupt");
        std::fs::write(d.join("elect.meta"), b"garbage\n").unwrap();
        let p = FileElectorPersist::new(&d);
        assert_eq!(p.load(), (0, None));
        let _ = std::fs::remove_dir_all(&d);
    }
}
