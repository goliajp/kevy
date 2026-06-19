//! `Scope` — one declared `[[cluster.scope]]` entry. Pure data; the
//! ownership table holds a vec of these.

/// One scope declaration: a key-prefix slice owned by `writer`, with
/// an optional `fallback` server that takes over writes when the
/// writer is flagged DOWN by `kevy-elect`.
///
/// The prefix is `Vec<u8>` (not `String`) because keys are arbitrary
/// bytes in kevy; restricting to UTF-8 would be a stricter contract
/// than the RESP wire offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub(crate) prefix: Vec<u8>,
    pub(crate) writer: String,
    pub(crate) fallback: Option<String>,
}

impl Scope {
    /// Build a minimal scope: prefix + writer. Add a fallback via
    /// [`Self::with_fallback`] if F4 is in play.
    #[must_use]
    pub fn new(prefix: Vec<u8>, writer: String) -> Self {
        Self { prefix, writer, fallback: None }
    }

    /// Declare a fallback node-id. When the writer is flagged DOWN by
    /// `kevy-elect`, the fallback starts accepting writes for this
    /// scope (F4). The fallback is one specific server — not "any
    /// alive node" — so its identity is operator-visible and not the
    /// cluster's discretion.
    #[must_use]
    pub fn with_fallback(mut self, fallback: String) -> Self {
        self.fallback = Some(fallback);
        self
    }

    /// Key-prefix slice this scope owns. Lifetime tied to the scope
    /// (not a clone) so longest-prefix routing avoids allocation per
    /// lookup.
    #[must_use]
    pub fn prefix(&self) -> &[u8] {
        &self.prefix
    }

    /// Declared writer's node id.
    #[must_use]
    pub fn writer(&self) -> &str {
        &self.writer
    }

    /// Declared fallback's node id, if any. `None` means "no
    /// fallback" — when the writer is DOWN, writes for this scope
    /// fail (the operator chose availability < strict ownership).
    #[must_use]
    pub fn fallback(&self) -> Option<&str> {
        self.fallback.as_deref()
    }

    /// `true` if `key` starts with this scope's prefix.
    #[must_use]
    pub fn matches(&self, key: &[u8]) -> bool {
        key.starts_with(&self.prefix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_starts_with() {
        let s = Scope::new(b"app:billing:".to_vec(), "w1".to_string());
        assert!(s.matches(b"app:billing:invoice:42"));
        assert!(s.matches(b"app:billing:"));
        assert!(!s.matches(b"app:auth:user:1"));
        assert!(!s.matches(b"app:billin")); // shorter than prefix
    }

    #[test]
    fn with_fallback_sets_fallback() {
        let s = Scope::new(b"p:".to_vec(), "w".to_string()).with_fallback("f".to_string());
        assert_eq!(s.fallback(), Some("f"));
    }

    #[test]
    fn empty_prefix_matches_anything() {
        // Edge case: an operator declaring an empty prefix claims the
        // entire keyspace. Useful for "single-writer cluster" config
        // where you want one node to own everything by default.
        let s = Scope::new(Vec::new(), "w".to_string());
        assert!(s.matches(b"anything"));
        assert!(s.matches(b""));
    }
}
