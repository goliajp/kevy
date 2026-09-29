//! Public store types split from `lib.rs` (500-LOC rule):
//! [`RenameOutcome`] / [`StoreError`] / [`EvictionPolicy`].

use core::fmt;

/// Outcome of [`Store::rename`](crate::Store::rename) and
/// [`Store::rename_nx`](crate::Store::rename_nx) — three-way result so the
/// dispatch layer can pick the right RESP frame (`+OK` / `-ERR no such
/// key` / `:0` for `RENAMENX`-with-existing-dst).
///
/// ```
/// use kevy_store::{RenameOutcome, SetCondition, Store};
/// let mut s = Store::new();
/// s.set(b"a", b"1".to_vec(), None, SetCondition::Always);
/// s.set(b"b", b"2".to_vec(), None, SetCondition::Always);
/// assert_eq!(s.rename_nx(b"a", b"b"), RenameOutcome::DstExists);
/// assert_eq!(s.rename(b"a", b"b"), RenameOutcome::Renamed);
/// assert_eq!(s.rename(b"a", b"c"), RenameOutcome::NoSuchSrc);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RenameOutcome {
    /// Source removed, destination created (overwriting any prior dst).
    Renamed,
    /// Source key doesn't exist.
    NoSuchSrc,
    /// `RENAMENX` only — destination already exists, no rename done.
    DstExists,
}

/// Operation errors surfaced to the command layer.
///
/// `Display` is the lowercase description; [`StoreError::as_wire`] is the
/// Redis error reply text the command layer sends.
///
/// ```
/// use kevy_store::StoreError;
/// assert_eq!(StoreError::WrongType.to_string(), "wrong type for this operation");
/// assert!(StoreError::WrongType.as_wire().starts_with("WRONGTYPE "));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
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

impl StoreError {
    /// The Redis error reply text for this error, prefix included
    /// (`WRONGTYPE …`, `ERR …`, `OOM …`) — what a RESP layer writes after
    /// the `-`.
    ///
    /// ```
    /// assert_eq!(kevy_store::StoreError::NoSuchKey.as_wire(), "ERR no such key");
    /// ```
    pub fn as_wire(&self) -> &'static str {
        match self {
            Self::WrongType => "WRONGTYPE Operation against a key holding the wrong kind of value",
            Self::NotInteger => "ERR value is not an integer or out of range",
            Self::Overflow => "ERR increment or decrement would overflow",
            Self::OutOfRange => "ERR index out of range",
            Self::NoSuchKey => "ERR no such key",
            Self::NotFloat => "ERR value is not a valid float",
            Self::OutOfMemory => "OOM command not allowed when used memory > 'maxmemory'.",
        }
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::WrongType => "wrong type for this operation",
            Self::NotInteger => "value is not an integer or out of range",
            Self::Overflow => "increment or decrement would overflow",
            Self::OutOfRange => "index out of range",
            Self::NoSuchKey => "no such key",
            Self::NotFloat => "value is not a valid float",
            Self::OutOfMemory => "maxmemory reached and the eviction policy is noeviction",
        })
    }
}

impl core::error::Error for StoreError {}

/// Maxmemory eviction policy — the one type for it: `kevy-config` parses
/// `maxmemory-policy` into it and the store enforces it.
///
/// ```
/// use kevy_store::EvictionPolicy;
/// assert_eq!(EvictionPolicy::parse("ALLKEYS-LRU"), Some(EvictionPolicy::AllKeysLru));
/// assert_eq!(EvictionPolicy::VolatileTtl.as_str(), "volatile-ttl");
/// assert_eq!(EvictionPolicy::default(), EvictionPolicy::NoEviction);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
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
    const ALL: [Self; 8] = [
        Self::NoEviction,
        Self::AllKeysLru,
        Self::AllKeysLfu,
        Self::AllKeysRandom,
        Self::VolatileLru,
        Self::VolatileLfu,
        Self::VolatileRandom,
        Self::VolatileTtl,
    ];

    /// Canonical Redis-compatible name (`maxmemory-policy` value).
    ///
    /// ```
    /// assert_eq!(kevy_store::EvictionPolicy::AllKeysLfu.as_str(), "allkeys-lfu");
    /// ```
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NoEviction => "noeviction",
            Self::AllKeysLru => "allkeys-lru",
            Self::AllKeysLfu => "allkeys-lfu",
            Self::AllKeysRandom => "allkeys-random",
            Self::VolatileLru => "volatile-lru",
            Self::VolatileLfu => "volatile-lfu",
            Self::VolatileRandom => "volatile-random",
            Self::VolatileTtl => "volatile-ttl",
        }
    }

    /// Inverse of [`Self::as_str`], ASCII case-insensitive.
    ///
    /// ```
    /// use kevy_store::EvictionPolicy;
    /// assert_eq!(EvictionPolicy::parse("Volatile-Ttl"), Some(EvictionPolicy::VolatileTtl));
    /// assert_eq!(EvictionPolicy::parse("lru"), None);
    /// ```
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str().eq_ignore_ascii_case(s))
    }

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
