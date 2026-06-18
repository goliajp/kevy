//! `shards.meta` — the persisted shard-layout descriptor for a data dir.
//!
//! Per-shard persistence files (`aof-{i}.aof`, `dump-{i}.rdb`) are only
//! readable under the routing that wrote them: change the shard count *or*
//! the key→shard scheme and every key sits in the wrong file. This little
//! sidecar records both so bring-up can detect a mismatch and re-shard
//! (with a `.premigration` backup) instead of silently stranding keys.
//!
//! Format v2: line 1 = shard count, line 2 = routing tag. The v1 file
//! (embedded-store B2 sharding) was the bare count — [`read_shards_meta`]
//! still accepts it as `Routing::KevyHash`, and an old binary reading a v2
//! file fails its whole-string `parse::<usize>()`, treats the dir as legacy
//! and takes the lossless re-shard path. Both directions stay safe.

use std::io;
use std::path::Path;

/// Key→shard routing scheme recorded in `shards.meta`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Routing {
    /// FxFmix word hash (`kevy_hash::KevyHash`) — the default scheme.
    KevyHash,
    /// Redis-cluster slots: CRC16 of the `{hashtag}` & 16383, contiguous
    /// even ranges per shard. Used by single-node cluster mode so external
    /// clients can compute key placement.
    Slots,
}

impl Routing {
    fn tag(self) -> &'static str {
        match self {
            Routing::KevyHash => "kevyhash",
            Routing::Slots => "slots",
        }
    }
}

/// The shard layout a data dir's per-shard files were written under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShardsMeta {
    /// Number of shards (`aof-{0..n}.aof` / `dump-{0..n}.rdb`).
    pub n: usize,
    /// Key→shard scheme.
    pub routing: Routing,
}

/// Read `shards.meta` from `path`. `None` = no meta / unparseable (callers
/// treat the dir as a legacy layout). A v1 single-number file reads as
/// `Routing::KevyHash`; an unknown routing tag is *not* guessed at — the
/// file came from a newer kevy, so we fall back to `None` and the caller's
/// lossless legacy path rather than misroute every key.
pub fn read_shards_meta(path: &Path) -> Option<ShardsMeta> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    let n: usize = lines.next()?.trim().parse().ok()?;
    let routing = match lines.next().map(str::trim) {
        None | Some("" | "kevyhash") => Routing::KevyHash,
        Some("slots") => Routing::Slots,
        Some(_) => return None,
    };
    Some(ShardsMeta { n, routing })
}

/// Write `shards.meta` to `path` (v2: count, then routing tag).
pub fn write_shards_meta(path: &Path, meta: ShardsMeta) -> io::Result<()> {
    std::fs::write(path, format!("{}\n{}\n", meta.n, meta.routing.tag()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v2_round_trip() {
        let dir = std::env::temp_dir().join(format!("kevy-meta-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("shards.meta");
        for meta in [
            ShardsMeta { n: 1, routing: Routing::KevyHash },
            ShardsMeta { n: 8, routing: Routing::Slots },
        ] {
            write_shards_meta(&p, meta).unwrap();
            assert_eq!(read_shards_meta(&p), Some(meta));
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn v1_bare_count_reads_as_kevyhash() {
        let dir = std::env::temp_dir().join(format!("kevy-meta-v1-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("shards.meta");
        std::fs::write(&p, "4").unwrap();
        assert_eq!(
            read_shards_meta(&p),
            Some(ShardsMeta { n: 4, routing: Routing::KevyHash })
        );
        std::fs::write(&p, "4\nfuture-scheme\n").unwrap();
        assert_eq!(read_shards_meta(&p), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
