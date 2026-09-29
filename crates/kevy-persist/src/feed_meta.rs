//! CDC feed sidecars — the on-disk half of the `(generation,
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
//! Boot decision table ([`boot_position`]):
//!
//! | feed-{i}.gen | feed-{i}.meta        | result                     |
//! |--------------|----------------------|----------------------------|
//! | absent       | absent               | gen 1, offset 0 (fresh)    |
//! | G            | absent               | gen G+1, offset 0 (bumped) |
//! | G            | `G off` (matching)   | gen G, offset off (resume) |
//! | G            | mismatched/corrupt   | gen G+1, offset 0 (bumped) |
//!
//! ```
//! use kevy_persist::feed_meta::{boot_position, write_feed_meta};
//! use kevy_replicate::feed::FeedPosition;
//!
//! let dir = kevy_tmpdir::unique_dir("feed-cycle-doc");
//! let first = boot_position(&dir, 0)?; // fresh dir
//! write_feed_meta(&dir, 0, FeedPosition::new(first.generation, 10))?; // clean stop
//! assert_eq!(boot_position(&dir, 0)?, FeedPosition::new(first.generation, 10)); // resumed
//! let after_crash = boot_position(&dir, 0)?; // no marker this time: unclean
//! assert_ne!(after_crash.generation, first.generation);
//! assert_eq!(after_crash.offset, 0);
//! # std::fs::remove_dir_all(&dir)?;
//! # Ok::<(), std::io::Error>(())
//! ```

// Best-effort removal, on paths where the file is being abandoned.
// A file that will not delete is a stray the next sweep collects,
// and refusing here would abandon the rest of the cleanup.
#![expect(clippy::let_underscore_must_use, reason = "removing what is already meant to be gone")]

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use kevy_replicate::feed::{FeedPosition, fresh_generation};

fn gen_path(dir: &Path, shard: usize) -> PathBuf {
    dir.join(format!("feed-{shard}.gen"))
}

fn meta_path(dir: &Path, shard: usize) -> PathBuf {
    dir.join(format!("feed-{shard}.meta"))
}

/// Persist the generation high-water mark (fsynced — this write is
/// rare and MUST survive a crash).
///
/// ```
/// let dir = kevy_tmpdir::unique_dir("feedgen-doc");
/// kevy_persist::feed_meta::write_feed_gen(&dir, 0, 7)?;
/// assert_eq!(std::fs::read_to_string(dir.join("feed-0.gen"))?, "7");
/// # std::fs::remove_dir_all(&dir)?;
/// # Ok::<(), std::io::Error>(())
/// ```
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

/// Write the clean-shutdown continuity marker: the feed's tail, which
/// the next [`boot_position`] resumes at.
///
/// ```
/// use kevy_persist::feed_meta::{boot_position, write_feed_meta};
///
/// let dir = kevy_tmpdir::unique_dir("feedmeta-doc");
/// let tail = boot_position(&dir, 0)?;
/// write_feed_meta(&dir, 0, kevy_replicate::feed::FeedPosition::new(tail.generation, 42))?;
/// assert_eq!(boot_position(&dir, 0)?.offset, 42);
/// # std::fs::remove_dir_all(&dir)?;
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn write_feed_meta(dir: &Path, shard: usize, tail: FeedPosition) -> io::Result<()> {
    let tmp = dir.join(format!("feed-{shard}.meta.tmp"));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(format!("{} {}", tail.generation, tail.offset).as_bytes())?;
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

/// The position a shard's feed resumes at, per the boot decision table:
/// consume the continuity marker (it is deleted regardless of validity —
/// a crash between now and the next clean shutdown must read as
/// unclean), bump + persist the generation when continuity is broken.
/// The offset is 0 unless a clean-shutdown marker matched.
///
/// ```
/// let dir = kevy_tmpdir::unique_dir("feedboot-doc");
/// let at = kevy_persist::feed_meta::boot_position(&dir, 0)?;
/// assert_eq!(at.offset, 0);
/// # std::fs::remove_dir_all(&dir)?;
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn boot_position(dir: &Path, shard: usize) -> io::Result<FeedPosition> {
    let highwater: Option<u64> =
        fs::read_to_string(gen_path(dir, shard)).ok().and_then(|s| s.trim().parse().ok());
    let marker = read_meta(dir, shard);
    let _ = fs::remove_file(meta_path(dir, shard));
    let at = match (highwater, marker) {
        // Fresh dir and unclean boot both DRAW a random generation —
        // a generation is a history identity, not a counter. Fixed
        // starts (1) or increments (g+1) collide across nodes: every
        // fresh node called its history "1", a startup election and a
        // failover promotion both called theirs "2", and a replica's
        // stale cursor then passed the generation fence into offset
        // aliasing (the availgate failover wedge).
        (None, _) => FeedPosition::new(fresh_generation(0), 0),
        (Some(g), Some((mg, off))) if mg == g => FeedPosition::new(g, off),
        (Some(g), _) => FeedPosition::new(fresh_generation(g), 0),
    };
    // Persist the (possibly bumped, possibly fresh) generation as the
    // new high-water before serving anything under it.
    if Some(at.generation) != highwater {
        write_feed_gen(dir, shard, at.generation)?;
    }
    Ok(at)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        kevy_tmpdir::unique_dir("feedmeta")
    }

    #[test]
    fn fresh_dir_draws_a_random_gen() {
        let d = tmp();
        let b = boot_position(&d, 0).unwrap();
        assert_ne!(b.generation, 0);
        assert_eq!(b.offset, 0);
        // gen high-water persisted
        assert_eq!(fs::read_to_string(d.join("feed-0.gen")).unwrap(), b.generation.to_string());
        // Two fresh dirs must not share an identity (the "every fresh
        // node is gen 1" collision).
        let d2 = tmp();
        let b2 = boot_position(&d2, 0).unwrap();
        assert_ne!(b.generation, b2.generation);
        let _ = fs::remove_dir_all(&d);
        let _ = fs::remove_dir_all(&d2);
    }

    #[test]
    fn clean_shutdown_resumes_cursor() {
        let d = tmp();
        let b = boot_position(&d, 0).unwrap();
        write_feed_meta(&d, 0, FeedPosition::new(b.generation, 42)).unwrap();
        let b2 = boot_position(&d, 0).unwrap();
        assert_eq!(b2, FeedPosition::new(b.generation, 42));
        // marker consumed: a crash NOW must draw fresh next time
        let b3 = boot_position(&d, 0).unwrap();
        assert_ne!(b3.generation, b.generation);
        assert_ne!(b3.generation, 0);
        assert_eq!(b3.offset, 0);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn unclean_boot_bumps_and_persists_highwater() {
        let d = tmp();
        let g1 = boot_position(&d, 0).unwrap().generation;
        // no marker written (crash) → fresh identity
        let b = boot_position(&d, 0).unwrap();
        assert_ne!(b.generation, g1);
        assert_ne!(b.generation, 0);
        assert_eq!(fs::read_to_string(d.join("feed-0.gen")).unwrap(), b.generation.to_string());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn mismatched_marker_bumps() {
        let d = tmp();
        let g1 = boot_position(&d, 0).unwrap().generation;
        write_feed_meta(&d, 0, FeedPosition::new(99, 7)).unwrap(); // stale/corrupt marker
        let b = boot_position(&d, 0).unwrap();
        assert_ne!(b.generation, g1);
        assert_eq!(b.offset, 0);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn shards_are_independent() {
        let d = tmp();
        let g0 = boot_position(&d, 0).unwrap().generation;
        write_feed_meta(&d, 0, FeedPosition::new(g0, 10)).unwrap();
        let _ = boot_position(&d, 1).unwrap(); // fresh shard 1
        let b0 = boot_position(&d, 0).unwrap();
        assert_eq!(b0.offset, 10);
        let _ = fs::remove_dir_all(&d);
    }
}
