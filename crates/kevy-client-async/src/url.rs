//! URL parsing for the async client.
//!
//! Mirrors the schemes accepted by [`kevy_resp_client::RespClient::from_url`]
//! — `kevy://`, `redis://`, `tcp://`. The non-TCP `kevy-client` schemes
//! (`mem://`, `file://`) are NOT supported: those backends are in-process
//! embedded and have no async story (the embedded path is synchronous by
//! construction; wrapping it in async is strictly slower than the
//! blocking client).
//!
//! TODO: once a third client crate needs URL parsing this should be
//! extracted into a `kevy-url` stone (per user's three-tier code model:
//! "written for the second time" is the trigger to extract). Today we
//! have `kevy-resp-client::parse_url` + `kevy-client::url::parse_url` +
//! this — but the shapes differ (`ParsedUrl` vs `Target` enum vs
//! ours), and unifying is a separate refactor.

use std::io;

/// Parsed TCP-style URL.
#[derive(Debug, PartialEq, Eq)]
pub struct ParsedUrl {
    /// Hostname or IP literal.
    pub host: String,
    /// TCP port, defaults to 6379 if the URL omits the `:port`.
    pub port: u16,
    /// Optional db index from a `/N` path component. Only valid for
    /// `kevy://` and `redis://`; `tcp://` rejects any path.
    pub db: Option<u32>,
}

/// Parse a TCP-style URL.
///
/// Accepts: `kevy://`, `redis://`, `tcp://` — wire-protocol identical
/// (RESP2/3 over TCP). Rejects TLS schemes (kevy has no TLS),
/// userinfo (no AUTH), and unknown schemes.
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
        "mem" | "file" => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "{scheme}:// is an in-process embedded backend with no async \
                 story — use the blocking `kevy-client` crate instead"
            ),
        )),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown URL scheme '{other}://'"),
        )),
    }
}

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

fn parse_db_path(scheme: &str, path: Option<&str>) -> io::Result<Option<u32>> {
    match path {
        None | Some("") => Ok(None),
        Some(p) if scheme == "tcp" => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("tcp:// URL must not have a path: '/{p}'"),
        )),
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

    #[test]
    fn kevy_redis_tcp_schemes_resolve() {
        for url in ["kevy://h:6379", "redis://h:6379", "tcp://h:6379"] {
            let p = parse_url(url).unwrap();
            assert_eq!(p.host, "h");
            assert_eq!(p.port, 6379);
            assert_eq!(p.db, None);
        }
    }

    #[test]
    fn default_port_is_6379() {
        let p = parse_url("kevy://example.com").unwrap();
        assert_eq!(p.port, 6379);
    }

    #[test]
    fn kevy_url_carries_db_index() {
        let p = parse_url("kevy://h:6379/3").unwrap();
        assert_eq!(p.db, Some(3));
    }

    #[test]
    fn tcp_url_rejects_path() {
        let err = parse_url("tcp://h:6379/0").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn tls_schemes_rejected() {
        assert_eq!(
            parse_url("rediss://h:6379").unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        assert_eq!(
            parse_url("kevys://h:6379").unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
    }

    #[test]
    fn mem_and_file_rejected_with_pointer_to_blocking() {
        for url in ["mem://", "file:///x"] {
            let err = parse_url(url).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::Unsupported);
            assert!(err.to_string().contains("kevy-client"));
        }
    }

    #[test]
    fn auth_rejected() {
        assert_eq!(
            parse_url("redis://u:p@h:6379").unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
    }
}
