//! `shards.meta` — the persisted shard-layout descriptor for a data dir.
//!
//! Per-shard persistence files (`aof-{i}.aof`, `dump-{i}.rdb`) are only
//! readable under the routing that wrote them: change the shard count *or*
//! the key→shard scheme and every key sits in the wrong file. This little
//! sidecar records both so bring-up can detect a mismatch and re-shard
//! (with a `.premigration` backup) instead of silently stranding keys.
//!
//! Format v2: line 1 = shard count, line 2 = routing tag. The v1 file
//! (embedded-store B2 sharding) was the bare count — [`ShardsMeta::read`]
//! still accepts it as `Routing::KevyHash`, and an old binary reading a v2
//! file fails its whole-string `parse::<usize>()`, treats the dir as legacy
//! and takes the lossless re-shard path. Both directions stay safe.

use std::io;
use std::path::Path;

/// Key→shard routing scheme recorded in `shards.meta`.
///
/// ```
/// use kevy_persist::{Routing, ShardsMeta};
///
/// let dir = kevy_tmpdir::unique_dir("routing-doc");
/// let path = dir.join("shards.meta");
/// ShardsMeta::new(4, Routing::Slots).write(&path)?;
/// // bring-up compares the recorded scheme with the one it will run
/// let recorded = ShardsMeta::read(&path).map(|m| m.routing);
/// assert_ne!(recorded, Some(Routing::default()), "a re-shard is due");
/// # std::fs::remove_dir_all(&dir)?;
/// # Ok::<(), std::io::Error>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum Routing {
    /// FxFmix word hash (`kevy_hash::KevyHash`) — the default scheme.
    ///
    /// ```
    /// use kevy_persist::{Routing, ShardsMeta};
    ///
    /// let dir = kevy_tmpdir::unique_dir("routing-kevyhash-doc");
    /// let path = dir.join("shards.meta");
    /// std::fs::write(&path, "4")?; // a v1 file: the bare count
    /// assert_eq!(ShardsMeta::read(&path).map(|m| m.routing), Some(Routing::KevyHash));
    /// # std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    #[default]
    KevyHash,
    /// Redis-cluster slots: CRC16 of the `{hashtag}` & 16383, contiguous
    /// even ranges per shard. Used by single-node cluster mode so external
    /// clients can compute key placement.
    ///
    /// ```
    /// use kevy_persist::{Routing, ShardsMeta};
    ///
    /// let dir = kevy_tmpdir::unique_dir("routing-slots-doc");
    /// let path = dir.join("shards.meta");
    /// ShardsMeta::new(3, Routing::Slots).write(&path)?;
    /// assert_eq!(std::fs::read_to_string(&path)?, "3\nslots\n");
    /// # std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    Slots,
}

impl Routing {
    pub(crate) fn tag(self) -> &'static str {
        match self {
            Routing::KevyHash => "kevyhash",
            Routing::Slots => "slots",
        }
    }
}

/// The shard layout a data dir's per-shard files were written under.
///
/// ```
/// use kevy_persist::{Routing, ShardsMeta};
///
/// let meta = ShardsMeta::new(4, Routing::Slots);
/// assert_eq!((meta.n, meta.routing), (4, Routing::Slots));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ShardsMeta {
    /// Number of shards (`aof-{0..n}.aof` / `dump-{0..n}.rdb`).
    ///
    /// ```
    /// use kevy_persist::{Routing, ShardsMeta, layout};
    ///
    /// let meta = ShardsMeta::new(2, Routing::KevyHash);
    /// let dir = std::path::Path::new("/data");
    /// let logs: Vec<_> = (0..meta.n).map(|i| layout::aof_path(dir, i)).collect();
    /// assert_eq!(logs, [dir.join("aof-0.aof"), dir.join("aof-1.aof")]);
    /// ```
    pub n: usize,
    /// Key→shard scheme.
    ///
    /// ```
    /// use kevy_persist::{Routing, ShardsMeta};
    ///
    /// let dir = kevy_tmpdir::unique_dir("meta-routing-doc");
    /// let path = dir.join("shards.meta");
    /// ShardsMeta::new(1, Routing::Slots).write(&path)?;
    /// assert_eq!(ShardsMeta::read(&path).map(|m| m.routing), Some(Routing::Slots));
    /// # std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub routing: Routing,
}

impl ShardsMeta {
    /// `n` shards routed by `routing`. The count has no default: it is the
    /// layout.
    ///
    /// ```
    /// let meta = kevy_persist::ShardsMeta::new(1, kevy_persist::Routing::KevyHash);
    /// assert_eq!(meta.n, 1);
    /// ```
    pub const fn new(n: usize, routing: Routing) -> Self {
        Self { n, routing }
    }
}

impl ShardsMeta {
    /// Read `shards.meta` from `path`. `None` = no meta / unparseable
    /// (callers treat the dir as a legacy layout). A v1 single-number file
    /// reads as `Routing::KevyHash`; an unknown routing tag is *not* guessed
    /// at — the file came from a newer kevy, so we fall back to `None` and
    /// the caller's lossless legacy path rather than misroute every key.
    ///
    /// ```
    /// let dir = kevy_tmpdir::unique_dir("shardsmeta-read-doc");
    /// assert_eq!(kevy_persist::ShardsMeta::read(&dir.join("shards.meta")), None);
    /// # std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn read(path: &Path) -> Option<ShardsMeta> {
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

    /// Write this layout to `path` as `shards.meta` (v2: count, then
    /// routing tag).
    ///
    /// ```
    /// use kevy_persist::{Routing, ShardsMeta};
    ///
    /// let dir = kevy_tmpdir::unique_dir("shardsmeta-write-doc");
    /// let meta = ShardsMeta::new(4, Routing::Slots);
    /// meta.write(&dir.join("shards.meta"))?;
    /// assert_eq!(ShardsMeta::read(&dir.join("shards.meta")), Some(meta));
    /// # std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn write(&self, path: &Path) -> io::Result<()> {
        std::fs::write(path, format!("{}\n{}\n", self.n, self.routing.tag()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v2_round_trip() {
        let dir = kevy_tmpdir::unique_dir("shardsmeta");
        let p = dir.join("shards.meta");
        for meta in [
            ShardsMeta { n: 1, routing: Routing::KevyHash },
            ShardsMeta { n: 8, routing: Routing::Slots },
        ] {
            meta.write(&p).unwrap();
            assert_eq!(ShardsMeta::read(&p), Some(meta));
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn v1_bare_count_reads_as_kevyhash() {
        let dir = kevy_tmpdir::unique_dir("shardsmeta");
        let p = dir.join("shards.meta");
        std::fs::write(&p, "4").unwrap();
        assert_eq!(ShardsMeta::read(&p), Some(ShardsMeta { n: 4, routing: Routing::KevyHash }));
        std::fs::write(&p, "4\nfuture-scheme\n").unwrap();
        assert_eq!(ShardsMeta::read(&p), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
