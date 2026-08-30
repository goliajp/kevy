//! Connection-URL parsing shared by every kevy client crate.
//!
//! Accepts the TCP schemes `kevy://`, `redis://`, and `tcp://` (wire-
//! protocol identical — RESP over TCP), an optional `/N` db path on
//! the semantic schemes, and rejects TLS schemes (kevy has no TLS)
//! and userinfo (kevy has no AUTH). Tiny by design — a full URL crate
//! would be a crates.io dependency, against the 0-dep charter.

use std::io;

/// Parsed URL pieces — scheme validated, host/port resolved, optional
/// db index extracted.
#[derive(Debug, PartialEq, Eq)]
pub struct ParsedUrl {
    /// Hostname or IP literal.
    pub host: String,
    /// TCP port; defaults to 6379 (Redis convention) when the URL
    /// omits `:port`.
    pub port: u16,
    /// Optional db index from a `/N` path component. Only valid for
    /// `kevy://` and `redis://`; `tcp://` accepts but ignores a path.
    pub db: Option<u32>,
}

/// Parse a TCP-style connection URL. See the module doc for the
/// accepted shapes.
pub fn parse_url(url: &str) -> io::Result<ParsedUrl> {
    let (scheme, rest) = split_scheme(url)?;
    if rest.contains('@') {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "userinfo (user:pass@host) is unsupported — kevy has no AUTH",
        ));
    }
    let (authority, path) = match rest.split_once('/') {
        Some((auth, p)) => (auth, Some(p)),
        None => (rest, None),
    };
    let (host, port) = parse_authority(authority)?;
    let db = parse_db_path(scheme, path)?;
    Ok(ParsedUrl { host, port, db })
}

/// Validate the URL scheme and return `(scheme, rest)` where `rest` is
/// everything past `://`. Rejects TLS schemes (kevy has no TLS) and
/// unknown schemes.
fn split_scheme(url: &str) -> io::Result<(&str, &str)> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "URL missing '://'"))?;
    match scheme {
        "kevy" | "redis" | "tcp" => Ok((scheme, rest)),
        "rediss" | "kevys" => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "TLS schemes (rediss://, kevys://) are unsupported — kevy has no TLS",
        )),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown URL scheme '{other}://'"),
        )),
    }
}

/// Parse `host[:port]` — defaulting to port 6379 (Redis convention)
/// when the colon is absent. Empty hosts are rejected.
fn parse_authority(authority: &str) -> io::Result<(String, u16)> {
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => {
            let port: u16 = p.parse().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, format!("bad port: {p}"))
            })?;
            (h.to_string(), port)
        }
        None => (authority.to_string(), 6379),
    };
    if host.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty host"));
    }
    Ok((host, port))
}

/// Optional DB index from the path component. `tcp://` is a raw-socket
/// URL that **ignores** any `/db` (no `SELECT`), per the client contract
/// (docs/client-contract.md §1.1); `kevy://` and `redis://` honour `/N`.
fn parse_db_path(scheme: &str, path: Option<&str>) -> io::Result<Option<u32>> {
    match path {
        None | Some("") => Ok(None),
        Some(_) if scheme == "tcp" => Ok(None),
        Some(p) => {
            let n: u32 = p.parse().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("bad db index: '{p}' (expected a non-negative integer)"),
                )
            })?;
            Ok(Some(n))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(u: &str) -> ParsedUrl {
        parse_url(u).unwrap_or_else(|e| panic!("{u}: {e}"))
    }

    #[test]
    fn kevy_redis_tcp_schemes_all_resolve() {
        for url in ["kevy://localhost:6379", "redis://localhost:6379", "tcp://localhost:6379"] {
            let p = parse(url);
            assert_eq!(p.host, "localhost");
            assert_eq!(p.port, 6379);
            assert_eq!(p.db, None);
        }
    }

    #[test]
    fn default_port_is_6379_when_omitted() {
        let p = parse("kevy://example.com");
        assert_eq!(p.host, "example.com");
        assert_eq!(p.port, 6379);
    }

    #[test]
    fn db_path_segment_parsed() {
        assert_eq!(parse("kevy://h:1/0").db, Some(0));
        assert_eq!(parse("redis://h:1/3").db, Some(3));
        assert_eq!(parse("kevy://h").db, None);
        assert_eq!(parse("kevy://h/").db, None);
    }

    #[test]
    fn tls_schemes_rejected() {
        let err = parse_url("rediss://h:6379").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
        let err = parse_url("kevys://h:6379").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    }

    #[test]
    fn auth_userinfo_rejected() {
        let err = parse_url("kevy://user:pass@h:6379").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    }

    #[test]
    fn unknown_scheme_rejected() {
        let err = parse_url("memcached://h:11211").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn missing_scheme_rejected() {
        assert!(parse_url("localhost:6379").is_err());
    }

    #[test]
    fn tcp_with_path_ignored() {
        // tcp:// is the raw form: it accepts but IGNORES a /db (no SELECT),
        // per the client contract (docs/client-contract.md §1.1). db is None.
        let parsed = parse("tcp://h:6379/0");
        assert_eq!(parsed.db, None);
    }

    #[test]
    fn bad_port_rejected() {
        assert!(parse_url("kevy://h:notaport").is_err());
        assert!(parse_url("kevy://h:99999").is_err()); // > u16::MAX
    }

    #[test]
    fn bad_db_rejected() {
        assert!(parse_url("kevy://h/abc").is_err());
        assert!(parse_url("kevy://h/-1").is_err());
    }

    #[test]
    fn empty_host_rejected() {
        assert!(parse_url("kevy://:6379").is_err());
    }
}
