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
    #[default]
    Background,
    /// Caller-driven via [`crate::Store::tick`]. Required for WASM
    /// targets (no threads) and single-threaded apps that don't want a
    /// background worker.
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
    #[default]
    IfAbsent,
    /// Overwrite an existing destination (`COPY … REPLACE`).
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
    /// Index terms only; phrase queries match their words anywhere in
    /// the document.
    #[default]
    Omit,
    /// Record token offsets, so phrase queries verify adjacency.
    Record,
}
