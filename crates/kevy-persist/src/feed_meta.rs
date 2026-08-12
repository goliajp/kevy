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
        // Fresh dir and unclean boot both DRAW a random generation —
        // a generation is a history identity, not a counter. Fixed
        // starts (1) or increments (g+1) collide across nodes: every
        // fresh node called its history "1", a startup election and a
        // failover promotion both called theirs "2", and a replica's
        // stale cursor then passed the generation fence into offset
        // aliasing (availgate failover wedge, 2026-08-12).
        (None, _) => FeedBoot { generation: fresh_generation(0), next_offset: 0 },
        (Some(g), Some((mg, off))) if mg == g => FeedBoot { generation: g, next_offset: off },
        (Some(g), _) => FeedBoot { generation: fresh_generation(g), next_offset: 0 },
    };
    // Persist the (possibly bumped, possibly fresh) generation as the
    // new high-water before serving anything under it.
    if Some(boot.generation) != highwater {
        write_feed_gen(dir, shard, boot.generation)?;
    }
    Ok(boot)
}

/// Random nonzero u64 distinct from `old` — mirror of
/// `kevy_replicate::feed::fresh_generation` (kevy-persist does not
/// depend on kevy-replicate; the ~8 lines are duplicated rather than
/// inverting the dependency for them). Identity, not crypto.
fn fresh_generation(old: u64) -> u64 {
    use std::hash::{BuildHasher, Hasher};
    loop {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(old);
        // Mask to 63 bits: generations ride RESP integers (REPL.TOKEN
        // / REPL.WAIT), which are i64 — the identity space is still 2^63.
        let g = h.finish() & (i64::MAX as u64);
        if g != 0 && g != old {
            return g;
        }
    }
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
        let b = load_feed_boot(&d, 0).unwrap();
        assert_ne!(b.generation, 0);
        assert_eq!(b.next_offset, 0);
        // gen high-water persisted
        assert_eq!(
            fs::read_to_string(d.join("feed-0.gen")).unwrap(),
            b.generation.to_string()
        );
        // Two fresh dirs must not share an identity (the "every fresh
        // node is gen 1" collision).
        let d2 = tmp();
        let b2 = load_feed_boot(&d2, 0).unwrap();
        assert_ne!(b.generation, b2.generation);
        let _ = fs::remove_dir_all(&d);
        let _ = fs::remove_dir_all(&d2);
    }

    #[test]
    fn clean_shutdown_resumes_cursor() {
        let d = tmp();
        let b = load_feed_boot(&d, 0).unwrap();
        write_feed_meta(&d, 0, b.generation, 42).unwrap();
        let b2 = load_feed_boot(&d, 0).unwrap();
        assert_eq!(b2, FeedBoot { generation: b.generation, next_offset: 42 });
        // marker consumed: a crash NOW must draw fresh next time
        let b3 = load_feed_boot(&d, 0).unwrap();
        assert_ne!(b3.generation, b.generation);
        assert_ne!(b3.generation, 0);
        assert_eq!(b3.next_offset, 0);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn unclean_boot_bumps_and_persists_highwater() {
        let d = tmp();
        let g1 = load_feed_boot(&d, 0).unwrap().generation;
        // no marker written (crash) → fresh identity
        let b = load_feed_boot(&d, 0).unwrap();
        assert_ne!(b.generation, g1);
        assert_ne!(b.generation, 0);
        assert_eq!(
            fs::read_to_string(d.join("feed-0.gen")).unwrap(),
            b.generation.to_string()
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn mismatched_marker_bumps() {
        let d = tmp();
        let g1 = load_feed_boot(&d, 0).unwrap().generation;
        write_feed_meta(&d, 0, 99, 7).unwrap(); // stale/corrupt marker
        let b = load_feed_boot(&d, 0).unwrap();
        assert_ne!(b.generation, g1);
        assert_eq!(b.next_offset, 0);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn shards_are_independent() {
        let d = tmp();
        let g0 = load_feed_boot(&d, 0).unwrap().generation;
        write_feed_meta(&d, 0, g0, 10).unwrap();
        let _ = load_feed_boot(&d, 1).unwrap(); // fresh shard 1
        let b0 = load_feed_boot(&d, 0).unwrap();
        assert_eq!(b0.next_offset, 10);
        let _ = fs::remove_dir_all(&d);
    }
}
