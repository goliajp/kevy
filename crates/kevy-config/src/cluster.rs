//! `[cluster]` section schema — single-node cluster mode + the
//! v3-cluster Phase 1.5 election peer list. Split out of
//! [`crate::schema`] so that file stays under the 500-LOC house
//! rule.
//!
//! Why the peer list is a flat comma-separated string (per
//! T1.5.4.5 decision (b)): TOML's `[[array_of_tables]]` would be
//! the idiomatic shape, but kevy-config's hand-rolled 0-dep
//! parser doesn't support it (and won't for one feature). A
//! `peers = "id@host:port,id@host:port,..."` value parses with
//! the existing flat KV grammar and the structural future-need
//! is bounded (kevy-elect's anti-scope forbids per-peer TLS,
//! auth, region etc).


/// `[cluster]` section — single-node cluster mode: keys route by
/// Redis-cluster slot (CRC16 `{hashtag}` & 16383) and every shard `i`
/// gets a second, deterministic listener at `port_base + i` that answers
/// wrong-shard keys with `-MOVED`, so stock cluster-aware clients
/// (`redis-benchmark --cluster`, `redis-cli -c`) can address shards
/// directly. The main SO_REUSEPORT port keeps full forward-anywhere
/// behaviour for non-cluster clients. Not hot-settable: the routing
/// scheme is a startup property of the data dir (`shards.meta`).
///
/// `Copy` was dropped in v1.19 once `peers` (a `Vec<PeerEntry>` for
/// `kevy-elect` Phase 1.5) joined this struct. Most call sites just
/// clone the per-tick `Config` snapshot via `Arc<Config>`, so the
/// Copy removal is invisible in the hot path.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClusterSection {
    /// Enable cluster mode. Default `false` (zero change).
    pub enabled: bool,
    /// First cluster port (shard `i` listens at `port_base + i`).
    /// `0` (default) = `server.port + 1`.
    pub port_base: u16,
    /// This node's stable id for the v3-cluster Phase 1.5
    /// election (≤ 32 B ASCII; unique across the cluster). Default
    /// empty — `kevy-elect` is dormant unless both `node_id` and
    /// `peers` are set (so v1.18-era configs need no edit).
    pub node_id: String,
    /// First election-control listener port; shard `i` binds at
    /// `elect_port_base + i`. Default `0` → `port_base + 100` (or
    /// `server.port + 101` when cluster mode is off).
    pub elect_port_base: u16,
    /// Operator-declared peer list for `kevy-elect`. Empty when
    /// failover is not configured. Each entry is one cluster node
    /// (including potentially *this* node — kevy-elect filters
    /// self by matching `node_id`).
    pub peers: Vec<PeerEntry>,
    /// Phase 3 / v1.21 `[[cluster.scope]]` declarations: each
    /// entry pins a key prefix to a writer node (and optional
    /// fallback). Empty when scope-based multi-writer is off.
    /// Same flat-string TOML shape rationale as `peers` —
    /// `scopes = "prefix=writer[|fallback],..."`.
    pub scopes: Vec<ScopeEntry>,
}

/// One scope declaration parsed from the TOML
/// `scopes = "prefix=writer[|fallback],..."` shape. Mirrors the
/// `kevy_scope::Scope` data; kept duplicated here so kevy-config
/// stays leaf-level and doesn't depend on kevy-scope (the dependency
/// direction is kevy-scope ← kevy-config consumer, not the other
/// way).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeEntry {
    /// Key-prefix bytes the scope owns. Bytes (not String) because
    /// kevy keys are arbitrary; common keys (`app:billing:`) are
    /// UTF-8 but the type signature stays honest.
    pub prefix: Vec<u8>,
    /// Declared writer's node id.
    pub writer: String,
    /// Optional fallback node id (F4).
    pub fallback: Option<String>,
}

impl ScopeEntry {
    /// Parse one `prefix=writer[|fallback]` token. The first `=`
    /// splits prefix from owner spec; the writer may carry an
    /// optional `|fallback` suffix. Returns `None` on any shape
    /// problem (missing `=`, empty fields, prefix containing `,`).
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
    pub fn parse_list(s: &str) -> Result<Vec<ScopeEntry>, String> {
        let mut out = Vec::new();
        for raw in s.split(',') {
            let token = raw.trim();
            if token.is_empty() {
                continue;
            }
            match Self::parse_one(token) {
                Some(p) => out.push(p),
                None => return Err(token.to_string()),
            }
        }
        Ok(out)
    }
}

/// One peer in the `kevy-elect` quorum, parsed from the TOML
/// shape `peers = "id@host:port,id@host:port,..."` (per
/// T1.5.4.5 decision (b) — a parser-extension-free representation
/// that works with kevy-config's flat KV-only TOML).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerEntry {
    /// Peer's stable node id.
    pub node_id: String,
    /// Peer's host (IPv4 dotted literal or DNS-resolvable name).
    pub host: String,
    /// Peer's election-control port (= peer's
    /// `cluster.elect_port_base + 0`, the shard 0 listener).
    pub port: u16,
}

impl PeerEntry {
    /// Parse one `id@host:port` token. Returns `None` on any shape
    /// problem (empty fields, non-numeric port, port overflow).
    pub fn parse_one(token: &str) -> Option<Self> {
        let (node_id, rest) = token.split_once('@')?;
        if node_id.is_empty() {
            return None;
        }
        let colon = rest.rfind(':')?;
        let host = &rest[..colon];
        if host.is_empty() {
            return None;
        }
        let port: u16 = rest[colon + 1..].parse().ok()?;
        Some(PeerEntry {
            node_id: node_id.to_string(),
            host: host.to_string(),
            port,
        })
    }

    /// Parse the `peers = "..."` value — a comma-separated list of
    /// `id@host:port` tokens. Empty + all-whitespace tokens are
    /// dropped silently (a trailing comma after the last entry is
    /// tolerated). Returns `Err(token)` on the first unparseable
    /// token, with the offending token in the error for diagnostic.
    pub fn parse_list(s: &str) -> Result<Vec<PeerEntry>, String> {
        let mut out = Vec::new();
        for raw in s.split(',') {
            let token = raw.trim();
            if token.is_empty() {
                continue;
            }
            match Self::parse_one(token) {
                Some(p) => out.push(p),
                None => return Err(token.to_string()),
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod peer_entry_tests {
    use super::*;

    #[test]
    fn parse_one_basic() {
        let p = PeerEntry::parse_one("node-1@10.0.0.1:6004").unwrap();
        assert_eq!(p.node_id, "node-1");
        assert_eq!(p.host, "10.0.0.1");
        assert_eq!(p.port, 6004);
    }

    #[test]
    fn parse_one_dns_host() {
        let p = PeerEntry::parse_one("primary@db-east.local:6105").unwrap();
        assert_eq!(p.host, "db-east.local");
        assert_eq!(p.port, 6105);
    }

    #[test]
    fn parse_one_rejects_empty_id_host_or_bad_port() {
        assert!(PeerEntry::parse_one("@host:6004").is_none());
        assert!(PeerEntry::parse_one("id@:6004").is_none());
        assert!(PeerEntry::parse_one("id@host:NaN").is_none());
        assert!(PeerEntry::parse_one("id@host:99999").is_none()); // u16 overflow
        assert!(PeerEntry::parse_one("no-at-or-colon").is_none());
    }

    #[test]
    fn parse_list_three_peers_trim_tolerated() {
        let s = "a@1.1.1.1:6004, b@1.1.1.2:6004 ,c@1.1.1.3:6004";
        let peers = PeerEntry::parse_list(s).unwrap();
        assert_eq!(peers.len(), 3);
        assert_eq!(peers[1].node_id, "b");
    }

    #[test]
    fn parse_list_trailing_comma_ok() {
        let peers = PeerEntry::parse_list("a@h:1,b@h:2,").unwrap();
        assert_eq!(peers.len(), 2);
    }

    #[test]
    fn parse_list_first_bad_token_errs() {
        let err = PeerEntry::parse_list("a@h:1,bad-token,c@h:3").unwrap_err();
        assert_eq!(err, "bad-token");
    }

    #[test]
    fn parse_list_empty_is_empty() {
        assert_eq!(PeerEntry::parse_list("").unwrap(), Vec::<PeerEntry>::new());
        assert_eq!(PeerEntry::parse_list("  ").unwrap(), Vec::<PeerEntry>::new());
    }
}

#[cfg(test)]
mod scope_entry_tests {
    use super::*;

    #[test]
    fn parse_one_writer_only() {
        let s = ScopeEntry::parse_one("app:billing:=embed-billing-1").unwrap();
        assert_eq!(s.prefix, b"app:billing:");
        assert_eq!(s.writer, "embed-billing-1");
        assert_eq!(s.fallback, None);
    }

    #[test]
    fn parse_one_writer_and_fallback() {
        let s = ScopeEntry::parse_one("app:billing:=embed-1|fb-server-eu").unwrap();
        assert_eq!(s.writer, "embed-1");
        assert_eq!(s.fallback.as_deref(), Some("fb-server-eu"));
    }

    #[test]
    fn parse_one_prefix_with_colons() {
        // Colon-heavy prefixes are the common case; only `=` and `,`
        // are reserved.
        let s = ScopeEntry::parse_one("ns:tenant:42:=w").unwrap();
        assert_eq!(s.prefix, b"ns:tenant:42:");
    }

    #[test]
    fn parse_one_rejects_empty_prefix_or_writer() {
        assert!(ScopeEntry::parse_one("=writer").is_none());
        assert!(ScopeEntry::parse_one("prefix=").is_none());
        assert!(ScopeEntry::parse_one("no-equals").is_none());
    }

    #[test]
    fn parse_one_rejects_empty_fallback_side() {
        assert!(ScopeEntry::parse_one("p=writer|").is_none());
        assert!(ScopeEntry::parse_one("p=|fb").is_none());
    }

    #[test]
    fn parse_one_rejects_embedded_comma() {
        // The split-on-comma in `parse_list` makes commas inside a
        // token a parse error — operator probably typo'd
        // `prefix=writer,fallback` instead of `prefix=writer|fallback`.
        assert!(ScopeEntry::parse_one("p=writer,other").is_none());
    }

    #[test]
    fn parse_list_two_scopes() {
        let v = ScopeEntry::parse_list("app:billing:=w-bill|fb, app:auth:=w-auth").unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].writer, "w-bill");
        assert_eq!(v[0].fallback.as_deref(), Some("fb"));
        assert_eq!(v[1].writer, "w-auth");
        assert!(v[1].fallback.is_none());
    }

    #[test]
    fn parse_list_first_bad_token_errs() {
        let err = ScopeEntry::parse_list("p1=w1,no-eq,p3=w3").unwrap_err();
        assert_eq!(err, "no-eq");
    }
}

