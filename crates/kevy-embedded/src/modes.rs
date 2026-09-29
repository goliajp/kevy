//! Named modes for [`crate::Store`] methods whose Redis command takes a
//! flag: the method reads the way the command does.

/// How the active TTL reaper runs.
///
/// ```
/// use kevy_embedded::{Config, TtlReaperMode};
///
/// assert_eq!(Config::default().ttl_reaper, TtlReaperMode::Background);
/// assert_eq!(Config::default().with_ttl_reaper_manual().ttl_reaper, TtlReaperMode::Manual);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum TtlReaperMode {
    /// Spawn a background thread that ticks at the configured interval
    /// (default 100 ms / 10 Hz, matching Redis's `hz=10`). Default.
    ///
    /// ```
    /// use std::time::Duration;
    /// use kevy_embedded::{Config, Store};
    ///
    /// let s = Store::open(Config::default())?;
    /// s.set_with_ttl(b"k", b"v", Duration::from_millis(1))?;
    /// // nobody calls tick(): the background thread reclaims the key
    /// for _ in 0..500 {
    ///     if s.info().expired_keys == 1 {
    ///         break;
    ///     }
    ///     std::thread::sleep(Duration::from_millis(10));
    /// }
    /// assert_eq!(s.info().expired_keys, 1);
    /// # Ok::<(), kevy_embedded::KevyError>(())
    /// ```
    #[default]
    Background,
    /// Caller-driven via [`crate::Store::tick`]. Required for WASM
    /// targets (no threads) and single-threaded apps that don't want a
    /// background worker.
    ///
    /// ```
    /// use std::time::Duration;
    /// use kevy_embedded::{Config, Store};
    ///
    /// let s = Store::open(Config::default().with_ttl_reaper_manual())?;
    /// s.set_with_ttl(b"k", b"v", Duration::from_millis(1))?;
    /// std::thread::sleep(Duration::from_millis(250));
    /// assert_eq!(s.info().expired_keys, 0, "nothing reaps until the caller ticks");
    /// for _ in 0..50 { s.tick(); }
    /// assert_eq!(s.info().expired_keys, 1);
    /// # Ok::<(), kevy_embedded::KevyError>(())
    /// ```
    Manual,
}

/// Whether `COPY` may overwrite an existing destination
/// ([`crate::Store::copy`]).
///
/// ```
/// use kevy_embedded::{Config, CopyMode, Store};
///
/// let s = Store::open(Config::default())?;
/// s.set(b"src", b"new")?;
/// s.set(b"dst", b"old")?;
/// assert!(!s.copy(b"src", b"dst", CopyMode::IfAbsent)?);
/// assert!(s.copy(b"src", b"dst", CopyMode::Replace)?);
/// assert_eq!(s.get(b"dst")?.as_deref(), Some(&b"new"[..]));
/// # Ok::<(), kevy_embedded::KevyError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum CopyMode {
    /// Copy only when the destination does not exist (`COPY` without
    /// `REPLACE`).
    ///
    /// ```
    /// use kevy_embedded::{Config, CopyMode, Store};
    ///
    /// let s = Store::open(Config::default())?;
    /// s.set(b"src", b"v")?;
    /// assert!(s.copy(b"src", b"fresh", CopyMode::IfAbsent)?, "an absent destination is written");
    /// s.set(b"taken", b"old")?;
    /// assert!(!s.copy(b"src", b"taken", CopyMode::IfAbsent)?);
    /// assert_eq!(s.get(b"taken")?.as_deref(), Some(&b"old"[..]));
    /// # Ok::<(), kevy_embedded::KevyError>(())
    /// ```
    #[default]
    IfAbsent,
    /// Overwrite an existing destination (`COPY … REPLACE`).
    ///
    /// ```
    /// use kevy_embedded::{Config, CopyMode, Store};
    ///
    /// let s = Store::open(Config::default())?;
    /// s.set(b"src", b"new")?;
    /// s.set(b"dst", b"old")?;
    /// assert!(s.copy(b"src", b"dst", CopyMode::Replace)?);
    /// assert_eq!(s.get(b"dst")?.as_deref(), Some(&b"new"[..]));
    /// # Ok::<(), kevy_embedded::KevyError>(())
    /// ```
    Replace,
}

/// Whether a text index records each token's position
/// ([`crate::Store::idx_create_text`]).
///
/// Positions let a phrase query verify that its words are adjacent; they
/// cost the positional side-channel's memory.
///
/// ```
/// assert_eq!(kevy_embedded::TokenPositions::default(), kevy_embedded::TokenPositions::Omit);
/// ```
#[cfg(feature = "text")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum TokenPositions {
    /// Index terms only. Term queries work as usual; a multi-word phrase
    /// query has no offsets to verify adjacency against and matches
    /// nothing.
    ///
    /// ```
    /// use kevy_embedded::{Config, Store, TokenPositions};
    ///
    /// let s = Store::open(Config::default())?;
    /// s.idx_create_text(b"ft", b"doc:", &[(b"body", 1.0)], TokenPositions::Omit, &[])?;
    /// s.hset(b"doc:1", &[(b"body", b"an engine written in rust")])?;
    /// assert_eq!(s.idx_match(b"ft", b"rust engine", 10)?.len(), 1);
    /// // no offsets were recorded, so no phrase can be verified
    /// assert!(s.idx_match(b"ft", b"\"written in rust\"", 10)?.is_empty());
    /// # Ok::<(), kevy_embedded::KevyError>(())
    /// ```
    #[default]
    Omit,
    /// Record token offsets, so phrase queries verify adjacency.
    ///
    /// ```
    /// use kevy_embedded::{Config, Store, TokenPositions};
    ///
    /// let s = Store::open(Config::default())?;
    /// s.idx_create_text(b"ft", b"doc:", &[(b"body", 1.0)], TokenPositions::Record, &[])?;
    /// s.hset(b"doc:1", &[(b"body", b"an engine written in rust")])?;
    /// assert!(s.idx_match(b"ft", b"\"rust engine\"", 10)?.is_empty(), "not adjacent");
    /// assert_eq!(s.idx_match(b"ft", b"\"written in rust\"", 10)?.len(), 1);
    /// # Ok::<(), kevy_embedded::KevyError>(())
    /// ```
    Record,
}
