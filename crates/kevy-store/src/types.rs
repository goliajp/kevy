//! Public store types split from `lib.rs` (500-LOC rule):
//! [`RenameOutcome`] / [`StoreError`] / [`EvictionPolicy`].

/// Outcome of [`Store::rename`] — three-way result so the dispatch
/// layer can pick the right RESP frame (`+OK` / `-ERR no such key` /
/// `:0` for `RENAMENX`-with-existing-dst).
#[derive(Debug, PartialEq, Eq)]
pub enum RenameOutcome {
    /// Source removed, destination created (overwriting any prior dst).
    Renamed,
    /// Source key doesn't exist.
    NoSuchSrc,
    /// `RENAMENX` only — destination already exists, no rename done.
    DstExists,
}

/// Operation errors surfaced to the command layer.
#[derive(Debug, PartialEq, Eq)]
pub enum StoreError {
    /// Key holds a different type than the command expects.
    WrongType,
    /// Value is not a base-10 integer (INCR family).
    NotInteger,
    /// Result would overflow `i64`.
    Overflow,
    /// Index outside the collection (LSET).
    OutOfRange,
    /// Key does not exist where the command requires one (LSET).
    NoSuchKey,
    /// Value is not a valid float (INCRBYFLOAT).
    NotFloat,
    /// `maxmemory` would be exceeded and the active eviction policy is
    /// [`EvictionPolicy::NoEviction`]. Surfaces as Redis's classic OOM error
    /// at the RESP layer.
    OutOfMemory,
}

/// Maxmemory eviction policy. Mirror of `kevy_config::EvictionPolicy` —
/// duplicated here so `kevy-store` stays a leaf crate (no `kevy-config` dep).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EvictionPolicy {
    /// Refuse writes once `maxmemory` is hit. Default.
    #[default]
    NoEviction,
    /// Approximated LRU across all keys.
    AllKeysLru,
    /// Approximated LFU across all keys.
    AllKeysLfu,
    /// Random key across all keys.
    AllKeysRandom,
    /// Approximated LRU across keys with a TTL.
    VolatileLru,
    /// Approximated LFU across keys with a TTL.
    VolatileLfu,
    /// Random key from those with a TTL.
    VolatileRandom,
    /// Key with the shortest remaining TTL.
    VolatileTtl,
}

impl EvictionPolicy {
    /// Whether the policy ranks candidates by LRU clock (read-touches matter).
    #[inline]
    pub fn uses_lru(self) -> bool {
        matches!(self, Self::AllKeysLru | Self::VolatileLru)
    }

    /// Whether the policy ranks candidates by LFU counter (read-touches and
    /// log-counter increments matter).
    #[inline]
    pub fn uses_lfu(self) -> bool {
        matches!(self, Self::AllKeysLfu | Self::VolatileLfu)
    }

    /// Whether the policy restricts eviction to keys that carry a TTL.
    #[inline]
    pub fn is_volatile(self) -> bool {
        matches!(
            self,
            Self::VolatileLru | Self::VolatileLfu | Self::VolatileRandom | Self::VolatileTtl
        )
    }
}

