//! The `[cluster]` list entries: [`PeerEntry`] (a quorum peer) and
//! [`ScopeEntry`] (a key prefix pinned to a writer), each with its
//! token parse and render.

use crate::error::ValueError;

/// One scope declaration parsed from the TOML
/// `scopes = "prefix=writer[|fallback],..."` shape. Mirrors the
/// `kevy_scope::Scope` data; kept duplicated here so kevy-config
/// stays leaf-level and doesn't depend on kevy-scope (the dependency
/// direction is kevy-scope ← kevy-config consumer, not the other
/// way).
///
/// There is no `Default`: a scope with no prefix and no writer is not a
/// scope. Build one with [`ScopeEntry::new`] or parse it.
///
/// ```
/// use kevy_config::ScopeEntry;
/// let s = ScopeEntry::new(b"app:billing:".to_vec(), "n1".into()).with_fallback("n2".into());
/// assert_eq!(s.to_token(), "app:billing:=n1|n2");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ScopeEntry {
    /// Key-prefix bytes the scope owns. Bytes (not String) because
    /// kevy keys are arbitrary; common keys (`app:billing:`) are
    /// UTF-8 but the type signature stays honest.
    ///
    /// ```
    /// let s = kevy_config::ScopeEntry::parse_one("app:billing:=n1").unwrap();
    /// assert_eq!(s.prefix, b"app:billing:");
    /// ```
    pub prefix: Vec<u8>,
    /// Declared writer's node id.
    ///
    /// ```
    /// let s = kevy_config::ScopeEntry::parse_one("app:billing:=n1|n2").unwrap();
    /// assert_eq!(s.writer, "n1");
    /// ```
    pub writer: String,
    /// Optional fallback node id (F4).
    ///
    /// ```
    /// use kevy_config::ScopeEntry;
    ///
    /// assert_eq!(ScopeEntry::parse_one("a:=n1|n2").unwrap().fallback.as_deref(), Some("n2"));
    /// assert_eq!(ScopeEntry::parse_one("a:=n1").unwrap().fallback, None);
    /// ```
    pub fallback: Option<String>,
}

impl ScopeEntry {
    /// A scope owning `prefix`, written by node `writer`, with no fallback.
    ///
    /// ```
    /// let s = kevy_config::ScopeEntry::new(b"p:".to_vec(), "w".into());
    /// assert_eq!((s.writer.as_str(), s.fallback), ("w", None));
    /// ```
    #[must_use]
    pub fn new(prefix: Vec<u8>, writer: String) -> Self {
        Self { prefix, writer, fallback: None }
    }

    /// The same scope with `fallback` taking writes while the writer is
    /// down.
    ///
    /// ```
    /// let s = kevy_config::ScopeEntry::new(b"p:".to_vec(), "w".into()).with_fallback("f".into());
    /// assert_eq!(s.fallback.as_deref(), Some("f"));
    /// ```
    #[must_use]
    pub fn with_fallback(mut self, fallback: String) -> Self {
        self.fallback = Some(fallback);
        self
    }

    /// Render back to the `prefix=writer[|fallback]` token shape —
    /// exact inverse of [`Self::parse_one`] for entries it produced.
    /// The prefix is parsed from TOML text, so it is UTF-8 whenever
    /// this is used on a config round-trip (`CONFIG REWRITE`).
    ///
    /// ```
    /// use kevy_config::ScopeEntry;
    ///
    /// let s = ScopeEntry::new(b"cache:".to_vec(), "n3".into());
    /// assert_eq!(s.to_token(), "cache:=n3");
    /// assert_eq!(ScopeEntry::parse_one(&s.to_token()), Some(s));
    /// ```
    pub fn to_token(&self) -> String {
        let prefix = String::from_utf8_lossy(&self.prefix);
        match &self.fallback {
            Some(fb) => format!("{prefix}={}|{fb}", self.writer),
            None => format!("{prefix}={}", self.writer),
        }
    }

    /// Parse one `prefix=writer[|fallback]` token. The first `=`
    /// splits prefix from owner spec; the writer may carry an
    /// optional `|fallback` suffix. Returns `None` on any shape
    /// problem (missing `=`, empty fields, prefix containing `,`).
    ///
    /// ```
    /// use kevy_config::ScopeEntry;
    ///
    /// let s = ScopeEntry::parse_one("app:billing:=n1|n2").unwrap();
    /// assert_eq!((s.writer.as_str(), s.fallback.as_deref()), ("n1", Some("n2")));
    /// assert_eq!(ScopeEntry::parse_one("app:billing:"), None); // no writer
    /// assert_eq!(ScopeEntry::parse_one("p:=n1|"), None); // empty fallback
    /// ```
    pub fn parse_one(token: &str) -> Option<Self> {
        // Reject commas inside the token — `parse_list` already
        // split on commas, so a comma here means the operator typed
        // `prefix=a,b` (ambiguous owner list); we treat that as a
        // parse error rather than silently take only `a`.
        if token.contains(',') {
            return None;
        }
        let (prefix, owners) = token.split_once('=')?;
        if prefix.is_empty() || owners.is_empty() {
            return None;
        }
        let (writer, fallback) = match owners.split_once('|') {
            Some((w, f)) if !w.is_empty() && !f.is_empty() => (w, Some(f.to_string())),
            Some(_) => return None, // `|` present but one side empty
            None => (owners, None),
        };
        Some(ScopeEntry {
            prefix: prefix.as_bytes().to_vec(),
            writer: writer.to_string(),
            fallback,
        })
    }

    /// Parse a `scopes = "..."` value — comma-separated list of
    /// `prefix=writer[|fallback]` tokens. Empty + whitespace-only
    /// tokens are dropped; trailing comma tolerated. Same
    /// error-on-first-bad-token contract as
    /// [`PeerEntry::parse_list`].
    ///
    /// ```
    /// use kevy_config::ScopeEntry;
    /// assert_eq!(ScopeEntry::parse_list("a:=n1, b:=n2|n3")?.len(), 2);
    /// assert!(ScopeEntry::parse_list("a:=n1,oops").is_err());
    /// # Ok::<(), kevy_config::ValueError>(())
    /// ```
    pub fn parse_list(s: &str) -> Result<Vec<ScopeEntry>, ValueError> {
        let mut out = Vec::new();
        for raw in s.split(',') {
            let token = raw.trim();
            if token.is_empty() {
                continue;
            }
            match Self::parse_one(token) {
                Some(p) => out.push(p),
                None => return Err(ValueError::new(format!("bad scope token: {token:?}"))),
            }
        }
        Ok(out)
    }
}

/// One peer in the `kevy-elect` quorum, parsed from the TOML
/// shape `peers = "id@host:port,id@host:port,..."` — a
/// parser-extension-free representation
/// that works with kevy-config's flat KV-only TOML.
///
/// The extended syntax adds an optional second port for the
/// **client-facing** address (used by `-MISDIRECTED writer is`
/// replies): `id@host:elect_port:client_port`. When the extended
/// form is used, kevy-elect still binds the elect_port, while
/// kevy-scope's MISDIRECTED encoder reports `host:client_port` to
/// the client so the client can actually reconnect to the writer.
/// Without the extended form, MISDIRECTED reports `host:elect_port`
/// (documented legacy behaviour, retained for compat).
///
/// A fourth field, `id@host:elect_port:client_port:repl_port_base`,
/// names where the peer accepts replicas when it does not use the
/// default base (client port + 10000). A node that follows a newly
/// elected primary dials this address.
///
/// There is no `Default`: a peer with no id, no host and port 0 is not a
/// peer, and a default that fills them in would be dialled. Build one
/// with [`PeerEntry::new`] or parse it.
///
/// ```
/// use kevy_config::PeerEntry;
/// let p = PeerEntry::new("n1".into(), "10.0.0.1".into(), 6204).with_client_port(6004);
/// assert_eq!(p.to_token(), "n1@10.0.0.1:6204:6004");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct PeerEntry {
    /// Peer's stable node id.
    ///
    /// ```
    /// let p = kevy_config::PeerEntry::parse_one("n2@10.0.0.2:6204").unwrap();
    /// assert_eq!(p.node_id, "n2");
    /// ```
    pub node_id: String,
    /// Peer's host (IPv4 dotted literal or DNS-resolvable name).
    ///
    /// ```
    /// let p = kevy_config::PeerEntry::parse_one("n2@db2.internal:6204").unwrap();
    /// assert_eq!(p.host, "db2.internal");
    /// ```
    pub host: String,
    /// Peer's election-control port (= peer's
    /// `cluster.elect_port_base + 0`, the shard 0 listener).
    ///
    /// ```
    /// let p = kevy_config::PeerEntry::parse_one("n2@10.0.0.2:6204:6004").unwrap();
    /// assert_eq!(p.port, 6204); // the election port, not the client one
    /// ```
    pub port: u16,
    /// Peer's client-facing TCP port (the port other
    /// kevy nodes / `redis-cli` connect to for normal operations).
    /// `None` = unset (legacy syntax `id@host:port`); MISDIRECTED
    /// replies fall back to `port` in that case. Set via extended
    /// syntax `id@host:elect_port:client_port`.
    ///
    /// ```
    /// use kevy_config::PeerEntry;
    ///
    /// assert_eq!(PeerEntry::parse_one("n2@h:6204:6004").unwrap().client_port, Some(6004));
    /// assert_eq!(PeerEntry::parse_one("n2@h:6204").unwrap().client_port, None);
    /// ```
    pub client_port: Option<u16>,
    /// Peer's `[replication].listen_port_base`. `None` = the default,
    /// client port + 10000.
    ///
    /// ```
    /// use kevy_config::PeerEntry;
    ///
    /// let p = PeerEntry::parse_one("n1@10.0.0.1:6204:6004:7100").unwrap();
    /// assert_eq!(p.repl_port_base, Some(7100));
    /// assert_eq!(PeerEntry::parse_one("n1@10.0.0.1:6204:6004").unwrap().repl_port_base, None);
    /// ```
    pub repl_port_base: Option<u16>,
}

impl PeerEntry {
    /// Peer `node_id` at `host`, election port `port`, in the legacy form
    /// (no client port, default replication base).
    ///
    /// ```
    /// let p = kevy_config::PeerEntry::new("n1".into(), "h".into(), 6204);
    /// assert_eq!((p.port, p.client_port, p.repl_port_base), (6204, None, None));
    /// ```
    #[must_use]
    pub fn new(node_id: String, host: String, port: u16) -> Self {
        Self { node_id, host, port, client_port: None, repl_port_base: None }
    }

    /// The same peer, with its client-facing port.
    ///
    /// ```
    /// let p = kevy_config::PeerEntry::new("n1".into(), "h".into(), 6204).with_client_port(6004);
    /// assert_eq!(p.client_port, Some(6004));
    /// ```
    #[must_use]
    pub fn with_client_port(mut self, client_port: u16) -> Self {
        self.client_port = Some(client_port);
        self
    }

    /// The same peer, with its replication listener base. Rendered only
    /// alongside a client port, as the token syntax requires.
    ///
    /// ```
    /// let p = kevy_config::PeerEntry::new("n1".into(), "h".into(), 6204)
    ///     .with_client_port(6004)
    ///     .with_repl_port_base(7100);
    /// assert_eq!(p.to_token(), "n1@h:6204:6004:7100");
    /// ```
    #[must_use]
    pub fn with_repl_port_base(mut self, repl_port_base: u16) -> Self {
        self.repl_port_base = Some(repl_port_base);
        self
    }

    /// Render back to the `id@host:port[:client_port[:repl_port_base]]` token shape —
    /// exact inverse of [`Self::parse_one`] for entries it produced.
    ///
    /// ```
    /// use kevy_config::PeerEntry;
    ///
    /// let p = PeerEntry::new("n2".into(), "10.0.0.2".into(), 6204);
    /// assert_eq!(p.to_token(), "n2@10.0.0.2:6204");
    /// // a replication base alone is not rendered: the syntax needs the client port first
    /// assert_eq!(p.clone().with_repl_port_base(7100).to_token(), "n2@10.0.0.2:6204");
    /// assert_eq!(PeerEntry::parse_one(&p.to_token()), Some(p));
    /// ```
    pub fn to_token(&self) -> String {
        let mut t = format!("{}@{}:{}", self.node_id, self.host, self.port);
        if let Some(cp) = self.client_port {
            t.push_str(&format!(":{cp}"));
            if let Some(rb) = self.repl_port_base {
                t.push_str(&format!(":{rb}"));
            }
        }
        t
    }

    /// Parse one peer token. Accepts three shapes:
    /// - **Legacy**: `id@host:port` (`port` = elect port).
    /// - **Extended**: `id@host:elect_port:client_port` (sets
    ///   `client_port` so MISDIRECTED reports a port the client
    ///   can actually connect to).
    /// - **With replication base**: `id@host:elect_port:client_port:repl_port_base`.
    ///
    /// Returns `None` on any shape problem (empty fields, non-numeric
    /// ports, port overflow).
    ///
    /// ```
    /// use kevy_config::PeerEntry;
    ///
    /// let p = PeerEntry::parse_one("n1@10.0.0.1:6204:6004").unwrap();
    /// assert_eq!((p.port, p.client_port), (6204, Some(6004)));
    /// assert_eq!(PeerEntry::parse_one("n1@10.0.0.1:70000"), None); // port overflow
    /// assert_eq!(PeerEntry::parse_one("10.0.0.1:6204"), None); // no node id
    /// ```
    pub fn parse_one(token: &str) -> Option<Self> {
        let (node_id, rest) = token.split_once('@')?;
        let mut fields = rest.split(':');
        let host = fields.next()?;
        if node_id.is_empty() || host.is_empty() {
            return None;
        }
        let ports = fields.map(|f| f.parse::<u16>().ok()).collect::<Option<Vec<u16>>>()?;
        let (port, client_port, repl_port_base) = match ports[..] {
            [elect] => (elect, None, None),
            [elect, client] => (elect, Some(client), None),
            [elect, client, repl] => (elect, Some(client), Some(repl)),
            _ => return None,
        };
        Some(PeerEntry {
            node_id: node_id.to_string(),
            host: host.to_string(),
            port,
            client_port,
            repl_port_base,
        })
    }

    /// Parse the `peers = "..."` value — a comma-separated list of
    /// `id@host:port` tokens. Empty + all-whitespace tokens are
    /// dropped silently (a trailing comma after the last entry is
    /// tolerated). Refuses on the first unparseable token, quoting it.
    ///
    /// ```
    /// use kevy_config::PeerEntry;
    /// assert_eq!(PeerEntry::parse_list("a@h:1, b@h:2,")?.len(), 2);
    /// let e = PeerEntry::parse_list("a@h:1,bad").unwrap_err();
    /// assert_eq!(e.to_string(), "bad peer token: \"bad\"");
    /// # Ok::<(), kevy_config::ValueError>(())
    /// ```
    pub fn parse_list(s: &str) -> Result<Vec<PeerEntry>, ValueError> {
        let mut out = Vec::new();
        for raw in s.split(',') {
            let token = raw.trim();
            if token.is_empty() {
                continue;
            }
            match Self::parse_one(token) {
                Some(p) => out.push(p),
                None => return Err(ValueError::new(format!("bad peer token: {token:?}"))),
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
#[path = "cluster_tests.rs"]
mod tests;
