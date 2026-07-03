//! v2.3 CDC feed sidecars — the on-disk half of the `(generation,
//! offset)` cursor contract.
//!
//! Two files per shard, with different write disciplines:
//!
//! - **`feed-{i}.gen`** — the generation high-water mark. Written +
//!   fsynced at every generation bump (rare: FLUSHALL, restore,
//!   unclean-boot recovery). Survives crashes, so a new generation is
//!   always numerically above every generation ever served from this
//!   data dir — the uniqueness half of the contract.
//! - **`feed-{i}.meta`** — the clean-shutdown continuity marker:
//!   `generation offset` on one line. Written on clean shutdown,
//!   **deleted at boot**. Present + valid at boot = the previous
//!   process stopped cleanly at that exact cursor → resume it
//!   (consumers see an unbroken stream). Absent = unclean stop (or
//!   fresh dir) → bump the generation, offsets restart at 0.
//!
//! Boot decision table ([`load_feed_boot`]):
//!
//! | feed-{i}.gen | feed-{i}.meta        | result                     |
//! |--------------|----------------------|----------------------------|
//! | absent       | absent               | gen 1, offset 0 (fresh)    |
//! | G            | absent               | gen G+1, offset 0 (bumped) |
//! | G            | `G off` (matching)   | gen G, offset off (resume) |
//! | G            | mismatched/corrupt   | gen G+1, offset 0 (bumped) |

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

fn gen_path(dir: &Path, shard: usize) -> PathBuf {
    dir.join(format!("feed-{shard}.gen"))
}

fn meta_path(dir: &Path, shard: usize) -> PathBuf {
    dir.join(format!("feed-{shard}.meta"))
}

/// The cursor a shard's feed resumes at, per the boot decision table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeedBoot {
    /// Generation to run at.
    pub generation: u64,
    /// Offset to resume from (0 unless a clean-shutdown marker matched).
    pub next_offset: u64,
}

/// Persist the generation high-water mark (fsynced — this write is
/// rare and MUST survive a crash).
pub fn write_feed_gen(dir: &Path, shard: usize, generation: u64) -> io::Result<()> {
    let tmp = dir.join(format!("feed-{shard}.gen.tmp"));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(generation.to_string().as_bytes())?;
        f.sync_all()?;
    }
    fs::rename(&tmp, gen_path(dir, shard))?;
    Ok(())
}

/// Write the clean-shutdown continuity marker.
pub fn write_feed_meta(dir: &Path, shard: usize, generation: u64, next_offset: u64) -> io::Result<()> {
    let tmp = dir.join(format!("feed-{shard}.meta.tmp"));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(format!("{generation} {next_offset}").as_bytes())?;
        f.sync_all()?;
    }
    fs::rename(&tmp, meta_path(dir, shard))?;
    Ok(())
}

fn read_meta(dir: &Path, shard: usize) -> Option<(u64, u64)> {
    let s = fs::read_to_string(meta_path(dir, shard)).ok()?;
    let mut it = s.split_whitespace();
    let g = it.next()?.parse().ok()?;
    let o = it.next()?.parse().ok()?;
    Some((g, o))
}

/// Run the boot decision table for one shard: consume the continuity
/// marker (it is deleted regardless of validity — a crash between now
/// and the next clean shutdown must read as unclean), bump + persist
/// the generation when continuity is broken.
pub fn load_feed_boot(dir: &Path, shard: usize) -> io::Result<FeedBoot> {
    let highwater: Option<u64> = fs::read_to_string(gen_path(dir, shard))
        .ok()
        .and_then(|s| s.trim().parse().ok());
    let marker = read_meta(dir, shard);
    let _ = fs::remove_file(meta_path(dir, shard));
    let boot = match (highwater, marker) {
        (None, _) => FeedBoot { generation: 1, next_offset: 0 },
        (Some(g), Some((mg, off))) if mg == g => FeedBoot { generation: g, next_offset: off },
        (Some(g), _) => FeedBoot { generation: g + 1, next_offset: 0 },
    };
    // Persist the (possibly bumped, possibly fresh) generation as the
    // new high-water before serving anything under it.
    if Some(boot.generation) != highwater {
        write_feed_gen(dir, shard, boot.generation)?;
    }
    Ok(boot)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "kevy-feedmeta-{}-{:?}",
            std::process::id(),
            std::time::Instant::now()
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn fresh_dir_starts_at_gen_one() {
        let d = tmp();
        assert_eq!(load_feed_boot(&d, 0).unwrap(), FeedBoot { generation: 1, next_offset: 0 });
        // gen high-water persisted
        assert_eq!(fs::read_to_string(d.join("feed-0.gen")).unwrap(), "1");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn clean_shutdown_resumes_cursor() {
        let d = tmp();
        let b = load_feed_boot(&d, 0).unwrap();
        write_feed_meta(&d, 0, b.generation, 42).unwrap();
        let b2 = load_feed_boot(&d, 0).unwrap();
        assert_eq!(b2, FeedBoot { generation: 1, next_offset: 42 });
        // marker consumed: a crash NOW must bump next time
        let b3 = load_feed_boot(&d, 0).unwrap();
        assert_eq!(b3, FeedBoot { generation: 2, next_offset: 0 });
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn unclean_boot_bumps_and_persists_highwater() {
        let d = tmp();
        let _ = load_feed_boot(&d, 0).unwrap(); // gen 1
        // no marker written (crash) → bump
        let b = load_feed_boot(&d, 0).unwrap();
        assert_eq!(b.generation, 2);
        assert_eq!(fs::read_to_string(d.join("feed-0.gen")).unwrap(), "2");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn mismatched_marker_bumps() {
        let d = tmp();
        let _ = load_feed_boot(&d, 0).unwrap(); // gen 1
        write_feed_meta(&d, 0, 99, 7).unwrap(); // stale/corrupt marker
        let b = load_feed_boot(&d, 0).unwrap();
        assert_eq!(b, FeedBoot { generation: 2, next_offset: 0 });
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn shards_are_independent() {
        let d = tmp();
        let _ = load_feed_boot(&d, 0).unwrap();
        write_feed_meta(&d, 0, 1, 10).unwrap();
        let _ = load_feed_boot(&d, 1).unwrap(); // fresh shard 1
        let b0 = load_feed_boot(&d, 0).unwrap();
        assert_eq!(b0.next_offset, 10);
        let _ = fs::remove_dir_all(&d);
    }
}
