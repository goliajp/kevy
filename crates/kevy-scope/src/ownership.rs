//! `OwnershipTable` — the immutable startup-built lookup structure.
//! Scopes are sorted by prefix length descending so the first match
//! IS the longest-prefix match (T3.7). Overlap is rejected at
//! construction time (T3.6).

use crate::{Routing, Scope};

/// Reasons [`OwnershipTable::new`] can refuse a list of scopes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnershipError {
    /// Two scopes share an identical prefix — ambiguous ownership.
    DuplicatePrefix {
        /// The prefix bytes (lossy UTF-8 in the formatted message).
        prefix: Vec<u8>,
    },
    /// One scope's prefix is a *strict* prefix of another's
    /// (e.g. `app:` and `app:billing:`). Even with longest-prefix
    /// match working at runtime, having both declared is almost
    /// always an operator mistake — the broader scope's writer
    /// would never see writes for the inner scope's keys. The
    /// linter rejects loudly rather than silently masking. To
    /// genuinely want a base + inner scope, use a single declaration
    /// at the innermost level.
    OverlappingPrefix {
        /// The longer (more specific) prefix in the conflict.
        inner: Vec<u8>,
        /// The shorter (broader) prefix that swallows the inner.
        outer: Vec<u8>,
    },
}

impl std::fmt::Display for OwnershipError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicatePrefix { prefix } => {
                write!(
                    f,
                    "duplicate scope prefix {:?}",
                    String::from_utf8_lossy(prefix)
                )
            }
            Self::OverlappingPrefix { inner, outer } => {
                write!(
                    f,
                    "overlapping scope prefixes: {:?} is contained by {:?}",
                    String::from_utf8_lossy(inner),
                    String::from_utf8_lossy(outer),
                )
            }
        }
    }
}

impl std::error::Error for OwnershipError {}

/// Immutable ownership table. Build once at startup from
/// `[[cluster.scope]]` config; share across reactor threads via
/// `Arc<OwnershipTable>`.
#[derive(Debug, Clone)]
pub struct OwnershipTable {
    /// Sorted by prefix length descending so the first
    /// `iter().find(|s| s.matches(key))` IS the longest-prefix match.
    /// Stable order on ties (none, after the duplicate-prefix check).
    scopes: Vec<Scope>,
}

impl OwnershipTable {
    /// Validate + sort the scope list. Rejects duplicate prefixes and
    /// strict overlap (T3.6 linter). `O(n²)` over the scope list,
    /// which is tiny (~ N scopes per cluster); no need for a trie at
    /// startup time.
    pub fn new(mut scopes: Vec<Scope>) -> Result<Self, OwnershipError> {
        // Duplicate check first (cheapest signal).
        for i in 0..scopes.len() {
            for j in (i + 1)..scopes.len() {
                if scopes[i].prefix == scopes[j].prefix {
                    return Err(OwnershipError::DuplicatePrefix {
                        prefix: scopes[i].prefix.clone(),
                    });
                }
            }
        }
        // Strict-overlap check: any pair where one is a prefix of the
        // other (but not equal — covered above) is a configuration
        // ambiguity.
        for i in 0..scopes.len() {
            for j in 0..scopes.len() {
                if i == j {
                    continue;
                }
                let a = &scopes[i].prefix;
                let b = &scopes[j].prefix;
                if a.len() < b.len() && b.starts_with(a) {
                    return Err(OwnershipError::OverlappingPrefix {
                        inner: b.clone(),
                        outer: a.clone(),
                    });
                }
            }
        }
        // Sort by prefix length descending: longest-prefix-match
        // = first-match after sort. `Reverse` flips the natural
        // length order without an explicit `cmp` closure.
        scopes.sort_by_key(|s| std::cmp::Reverse(s.prefix.len()));
        Ok(Self { scopes })
    }

    /// Iterate over declared scopes (read-only). Mostly useful for
    /// `INFO`-style telemetry.
    pub fn scopes(&self) -> &[Scope] {
        &self.scopes
    }

    /// T3.13 linter: scopes that declared no fallback. The server
    /// emits a `WARN` log per entry at boot so operators are aware
    /// that this scope has zero availability if its writer dies —
    /// not an error (the operator may have explicitly chosen
    /// availability < strict ownership), just a heads-up.
    pub fn scopes_without_fallback(&self) -> Vec<&Scope> {
        self.scopes.iter().filter(|s| s.fallback().is_none()).collect()
    }

    /// Number of declared scopes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.scopes.len()
    }

    /// `true` when no scopes are declared (the keyspace runs in
    /// pre-Phase-3 mode by default).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.scopes.is_empty()
    }

    /// Longest-prefix match: the most specific scope that owns
    /// `key`, or `None` when no scope matches.
    #[must_use]
    pub fn lookup(&self, key: &[u8]) -> Option<&Scope> {
        self.scopes.iter().find(|s| s.matches(key))
    }

    /// Route `key` from the local node's perspective. Treats the
    /// declared writer as the active owner; F4 fallback handling is
    /// layered on top in the server's cement (this fn doesn't know
    /// about live `kevy-elect` state — pure config view).
    #[must_use]
    pub fn route<'a>(&'a self, key: &[u8], self_node_id: &str) -> Routing<'a> {
        let Some(scope) = self.lookup(key) else {
            return Routing::Unknown;
        };
        if scope.writer() == self_node_id {
            Routing::Owned
        } else {
            Routing::Misdirected { target: scope.writer() }
        }
    }

    /// Fallback-aware routing: when `writer_down` is true for the
    /// matched scope's writer, the fallback (if declared) is treated
    /// as the active owner. Used by the cement after `kevy-elect`
    /// flags the writer DOWN (F4).
    ///
    /// `is_writer_down(node_id) -> bool` is a callback so kevy-scope
    /// stays elect-agnostic; the cement plugs in its live snapshot.
    pub fn route_with_fallback_state<'a, F>(
        &'a self,
        key: &[u8],
        self_node_id: &str,
        mut is_writer_down: F,
    ) -> Routing<'a>
    where
        F: FnMut(&str) -> bool,
    {
        let Some(scope) = self.lookup(key) else {
            return Routing::Unknown;
        };
        let active_owner = if is_writer_down(scope.writer()) {
            scope.fallback().unwrap_or(scope.writer())
        } else {
            scope.writer()
        };
        if active_owner == self_node_id {
            Routing::Owned
        } else {
            Routing::Misdirected { target: active_owner }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(prefix: &[u8], writer: &str) -> Scope {
        Scope::new(prefix.to_vec(), writer.to_string())
    }

    #[test]
    fn empty_table_routes_unknown() {
        let t = OwnershipTable::new(Vec::new()).unwrap();
        assert!(t.is_empty());
        assert_eq!(t.route(b"any-key", "node-1"), Routing::Unknown);
    }

    #[test]
    fn longest_prefix_wins() {
        let t = OwnershipTable::new(vec![
            // `app:` and `other:` — disjoint, no overlap → fine
            s(b"app:", "w-app"),
            s(b"other:", "w-other"),
        ])
        .unwrap();
        assert!(t.route(b"app:billing:invoice", "w-app").is_local_writer());
        assert!(t.route(b"other:settings", "w-other").is_local_writer());
        assert_eq!(t.route(b"unrelated", "any"), Routing::Unknown);
    }

    #[test]
    fn misdirected_carries_writer_id() {
        let t = OwnershipTable::new(vec![s(b"app:", "w-app")]).unwrap();
        let r = t.route(b"app:x", "some-other-node");
        assert_eq!(r.misdirected_target(), Some("w-app"));
    }

    #[test]
    fn overlap_rejected() {
        let err = OwnershipTable::new(vec![
            s(b"app:", "w-app"),
            s(b"app:billing:", "w-billing"),
        ])
        .unwrap_err();
        assert!(matches!(err, OwnershipError::OverlappingPrefix { .. }));
    }

    #[test]
    fn duplicate_rejected() {
        let err = OwnershipTable::new(vec![
            s(b"app:", "w-app"),
            s(b"app:", "w-app2"),
        ])
        .unwrap_err();
        assert!(matches!(err, OwnershipError::DuplicatePrefix { .. }));
    }

    #[test]
    fn fallback_active_when_writer_down() {
        let t = OwnershipTable::new(vec![
            Scope::new(b"app:".to_vec(), "w-app".to_string())
                .with_fallback("fb-1".to_string()),
        ])
        .unwrap();
        // Writer up: local node (fb-1) sees Misdirected.
        let r = t.route_with_fallback_state(b"app:x", "fb-1", |_| false);
        assert_eq!(r.misdirected_target(), Some("w-app"));
        // Writer down: fallback (fb-1) is now the active owner.
        let r = t.route_with_fallback_state(b"app:x", "fb-1", |id| id == "w-app");
        assert!(r.is_local_writer());
    }

    #[test]
    fn fallback_absent_falls_through_to_writer() {
        // No fallback declared → "writer DOWN" still names the writer
        // as the target (cement decides whether to refuse vs error).
        let t = OwnershipTable::new(vec![s(b"k:", "w-1")]).unwrap();
        let r = t.route_with_fallback_state(b"k:abc", "other", |id| id == "w-1");
        assert_eq!(r.misdirected_target(), Some("w-1"));
    }

    #[test]
    fn lookup_matches_longest_prefix_after_sort() {
        // Build the table with overlap-forbidden distinct prefixes;
        // longest-match still applies among disjoint declarations.
        let t = OwnershipTable::new(vec![
            s(b"a:", "wa"),
            s(b"b:", "wb"),
            s(b"abc:", "wabc"),
        ])
        .unwrap();
        assert_eq!(t.lookup(b"a:foo").map(Scope::writer), Some("wa"));
        assert_eq!(t.lookup(b"abc:foo").map(Scope::writer), Some("wabc"));
        assert_eq!(t.lookup(b"b:foo").map(Scope::writer), Some("wb"));
        assert!(t.lookup(b"nope").is_none());
    }
}
