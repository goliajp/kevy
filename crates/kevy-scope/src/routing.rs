//! `Routing` — the verdict for one `route(key, self_node_id)` call.
//! The server-cement layer translates this into one of:
//! - **Owned** → execute the write locally
//! - **Misdirected** → reply `-MISDIRECTED writer is <host:port>`
//! - **Unknown** → no scope matches; fall back to default behaviour
//!   (today: accept locally; v3.x may flip to reject, see the RFC).

/// Result of an [`crate::OwnershipTable::route`] lookup. Borrows the
/// writer/target ids out of the table — caller copies only when it
/// needs to log or encode them.
///
/// ```
/// use kevy_scope::{OwnershipTable, Routing, Scope};
///
/// let table = OwnershipTable::new(vec![Scope::new(b"app:".to_vec(), "w1".to_string())])?;
/// let r = table.route(b"app:k", "w2");
/// assert!(r.is_misdirected());
/// assert!(!r.is_local_writer());
/// assert_eq!(r.misdirected_target(), Some("w1"));
/// assert_eq!(table.route(b"app:k", "w1"), Routing::Owned);
/// assert_eq!(table.route(b"other:k", "w1"), Routing::Unknown);
/// # Ok::<(), kevy_scope::OwnershipError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Routing<'a> {
    /// `self_node_id` is the declared writer (or active fallback) for
    /// the matching scope. Execute the write locally.
    ///
    /// ```
    /// use kevy_scope::{OwnershipTable, Routing, Scope};
    ///
    /// let table = OwnershipTable::new(vec![Scope::new(b"app:".to_vec(), "w1".to_string())])?;
    /// assert_eq!(table.route(b"app:k", "w1"), Routing::Owned);
    /// # Ok::<(), kevy_scope::OwnershipError>(())
    /// ```
    Owned,
    /// Another node owns this write. The cement layer encodes
    /// `-MISDIRECTED writer is <writer>` to the wire so the client
    /// can follow. The `target` field is the *node id* — the server
    /// resolves that to `host:port` from its peer table at encode
    /// time (kevy-scope intentionally doesn't carry peer addrs).
    ///
    /// ```
    /// use kevy_scope::{OwnershipTable, Routing, Scope};
    ///
    /// let table = OwnershipTable::new(vec![Scope::new(b"app:".to_vec(), "w1".to_string())])?;
    /// assert_eq!(table.route(b"app:k", "w2"), Routing::Misdirected { target: "w1" });
    /// # Ok::<(), kevy_scope::OwnershipError>(())
    /// ```
    Misdirected {
        /// Node id of the actual writer (or active fallback) for
        /// this key's scope.
        ///
        /// ```
        /// use kevy_scope::{OwnershipTable, Routing, Scope};
        ///
        /// let s = Scope::new(b"app:".to_vec(), "w1".to_string()).with_fallback("f1".to_string());
        /// let table = OwnershipTable::new(vec![s])?;
        /// // with the writer down, the fallback is the node to follow
        /// let r = table.route_with_fallback_state(b"app:k", "w2", |node| node == "w1");
        /// let Routing::Misdirected { target } = r else { panic!("expected a redirect") };
        /// assert_eq!(target, "f1");
        /// # Ok::<(), kevy_scope::OwnershipError>(())
        /// ```
        target: &'a str,
    },
    /// No scope matched. Default policy is "accept locally" — the
    /// scope system is opt-in, so keys outside declared scopes
    /// behave like the pre-Phase-3 keyspace.
    ///
    /// ```
    /// use kevy_scope::{OwnershipTable, Routing, Scope};
    ///
    /// let table = OwnershipTable::new(vec![Scope::new(b"app:".to_vec(), "w1".to_string())])?;
    /// assert_eq!(table.route(b"cache:k", "w2"), Routing::Unknown);
    /// # Ok::<(), kevy_scope::OwnershipError>(())
    /// ```
    Unknown,
}

impl Routing<'_> {
    /// `true` when the current node owns the write.
    #[must_use]
    pub fn is_local_writer(&self) -> bool {
        matches!(self, Routing::Owned)
    }

    /// `true` for the `-MISDIRECTED` branch.
    #[must_use]
    pub fn is_misdirected(&self) -> bool {
        matches!(self, Routing::Misdirected { .. })
    }

    /// `Some(target)` for `Misdirected`, else `None`. Convenience for
    /// the cement layer's RESP encoder.
    #[must_use]
    pub fn misdirected_target(&self) -> Option<&str> {
        if let Routing::Misdirected { target } = self { Some(target) } else { None }
    }
}
