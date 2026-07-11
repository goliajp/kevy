//! URL parsing for the async client — a thin front over the canonical
//! parser in [`kevy_resp_client`], shared with the blocking clients.
//!
//! Accepts the TCP schemes (`kevy://`, `redis://`, `tcp://`). The
//! non-TCP `kevy-client` schemes (`mem://`, `file://`) are NOT
//! supported: those backends are in-process embedded and have no
//! async story (the embedded path is synchronous by construction;
//! wrapping it in async is strictly slower than the blocking client)
//! — they get a pointed error instead of the generic unknown-scheme
//! one.

use std::io;

pub use kevy_resp_client::ParsedUrl;

/// Parse a TCP-style URL. See the module doc for the accepted shapes.
pub fn parse_url(url: &str) -> io::Result<ParsedUrl> {
    if let Some((scheme @ ("mem" | "file"), _)) = url.split_once("://") {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "{scheme}:// is an in-process embedded backend with no async \
                 story — use the blocking `kevy-client` crate instead"
            ),
        ));
    }
    kevy_resp_client::parse_url(url)
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
    fn kevy_url_carries_db_index() {
        let p = parse_url("kevy://h:6379/3").unwrap();
        assert_eq!(p.db, Some(3));
    }

    #[test]
    fn embedded_schemes_get_the_pointed_error() {
        for url in ["mem://cache", "file:///tmp/kevy"] {
            let err = parse_url(url).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::Unsupported);
            assert!(err.to_string().contains("blocking"), "{err}");
        }
    }

    #[test]
    fn tls_and_userinfo_still_rejected() {
        assert!(parse_url("rediss://h:6379").is_err());
        assert!(parse_url("kevy://u:p@h:6379").is_err());
    }
}
