//! Global indexes: one index spread over the shards by value order, so a
//! read of one value range goes to the shards holding that range instead
//! of to every shard.
//!
//! An index is local by default: every shard holds the entries of the rows
//! it owns. A global index is cut into `P` partitions at `P - 1` split
//! points, byte strings in the index's own order encoding; partition `p`
//! holds the entries whose encoded value lies in `[splits[p-1],
//! splits[p])`, and lives on the shard [`partition_owner`] names. Both are
//! pure functions of the catalog, so any shard can place any value.

use crate::catalog::{Catalog, IndexKind, IndexSpec};

/// How an index is spread over the shards.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Partitioning {
    /// Every shard holds the entries of the rows it owns.
    #[default]
    Local,
    /// The entries are split by value order at these points, strictly
    /// increasing byte strings of the index's order encoding.
    Global {
        /// The `P - 1` split points.
        splits: Vec<Vec<u8>>,
    },
}

static LOCAL: Partitioning = Partitioning::Local;

impl Partitioning {
    /// Whether the index is split by value.
    pub fn is_global(&self) -> bool {
        matches!(self, Partitioning::Global { .. })
    }

    /// How many partitions there are: 1 for a local index.
    pub fn partitions(&self) -> usize {
        match self {
            Partitioning::Local => 1,
            Partitioning::Global { splits } => splits.len() + 1,
        }
    }

    /// The partition an encoded value belongs to (0 for a local index).
    ///
    /// ```
    /// use kevy_index::Partitioning;
    /// let p = Partitioning::Global { splits: vec![b"g".to_vec(), b"p".to_vec()] };
    /// assert_eq!(p.partition_of(b"apple"), 0);
    /// assert_eq!(p.partition_of(b"g"), 1);
    /// assert_eq!(p.partition_of(b"zebra"), 2);
    /// ```
    pub fn partition_of(&self, enc: &[u8]) -> usize {
        match self {
            Partitioning::Local => 0,
            Partitioning::Global { splits } => splits.partition_point(|s| s.as_slice() <= enc),
        }
    }
}

/// The shard partition `p` of the index `name` lives on, of `n` shards.
/// The name shifts the start, so the first partitions of different indexes
/// do not all land on shard 0.
///
/// ```
/// let n = 4;
/// let owners: Vec<usize> = (0..n).map(|p| kevy_index::partition_owner(b"by_age", p, n)).collect();
/// let mut sorted = owners.clone();
/// sorted.sort();
/// assert_eq!(sorted, [0, 1, 2, 3], "one partition per shard");
/// ```
pub fn partition_owner(name: &[u8], p: usize, n: usize) -> usize {
    // FNV-1a: stable across builds and versions, which a placement must be
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in name {
        h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    (p + (h % n as u64) as usize) % n
}

/// Why a global partitioning cannot go on this index.
fn global_guard(spec: &IndexSpec, splits: &[Vec<u8>]) -> Result<(), &'static str> {
    if !matches!(spec.kind, IndexKind::Range | IndexKind::Unique) {
        return Err("ERR PARTITION global requires KIND range|unique");
    }
    if splits.windows(2).any(|w| w[0] >= w[1]) {
        return Err("ERR SPLIT AT values must be strictly increasing");
    }
    Ok(())
}

impl Catalog {
    /// [`Catalog::create`] with a partitioning. A global one is refused by
    /// name where it cannot apply: kinds whose segments are not a
    /// `(value, key)` order, and split points out of order.
    pub fn create_with(
        &mut self,
        spec: IndexSpec,
        partitioning: Partitioning,
    ) -> Result<(), &'static str> {
        if let Partitioning::Global { splits } = &partitioning {
            global_guard(&spec, splits)?;
        }
        let name = spec.name.clone();
        self.create(spec)?;
        if partitioning.is_global() {
            self.parts.push((name, partitioning));
        }
        Ok(())
    }

    /// How the index `name` is spread (local when it is not global, or
    /// not there).
    pub fn partitioning(&self, name: &[u8]) -> &Partitioning {
        self.parts.iter().find(|(n, _)| n == name).map_or(&LOCAL, |(_, p)| p)
    }

    /// Re-split a global index (a rebuild re-sampled it). `false` when
    /// `name` is not a global index of this catalog, or `splits` are out
    /// of order.
    pub fn set_splits(&mut self, name: &[u8], splits: Vec<Vec<u8>>) -> bool {
        if splits.windows(2).any(|w| w[0] >= w[1]) {
            return false;
        }
        match self.parts.iter_mut().find(|(n, _)| n == name) {
            Some((_, p)) => {
                *p = Partitioning::Global { splits };
                true
            }
            None => false,
        }
    }
}
