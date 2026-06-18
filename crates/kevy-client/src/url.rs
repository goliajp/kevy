//! URL parsing + process-local embed-Store registry.
//!
//! Two opens with the same `mem://<name>` or `file:///path` URL share
//! the same backing [`kevy_embedded::Store`] — that's what makes embedded
//! pub/sub work end-to-end: the publisher's `Connection` and the
//! consumer's `Subscriber`, opened with the same URL, find the same bus.
//! Anonymous `mem://` (no name) skips the registry and stays per-call
//! isolated, preserving the v1.0/v1.1/v1.2 behaviour.

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use kevy_embedded::{Config, Store, WeakStore};

/// What [`parse_url`] resolves an input to.
#[derive(Debug, Clone)]
pub(crate) enum Target {
    /// `mem://` — anonymous, fresh `Store` each open, never shared.
    EmbedMemoryAnonymous,
    /// `mem://<name>` — shared by `<name>` across this process.
    EmbedMemoryNamed(String),
    /// `file:///abs/path` / `file://./rel/path` — shared by canonical path
    /// across this process; also persists to disk (snapshot + AOF).
    EmbedPersist(PathBuf),
    /// `kevy://…` / `redis://…` / `tcp://…` — delegate to RespClient.
    Remote(String),
}

pub(crate) fn parse_url(url: &str) -> io::Result<Target> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "URL missing '://'"))?;
    match scheme {
        "mem" => Ok(if rest.is_empty() {
            Target::EmbedMemoryAnonymous
        } else {
            Target::EmbedMemoryNamed(rest.to_string())
        }),
        "file" => {
            if rest.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "file:// URL must include a path (e.g. `file:///var/lib/myapp`)",
                ));
            }
            Ok(Target::EmbedPersist(PathBuf::from(rest)))
        }
        "kevy" | "redis" | "tcp" => Ok(Target::Remote(url.to_string())),
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

/// Process-local URL → `WeakStore` map. Entries auto-evict when their
/// last strong handle drops (the next `open` for that URL gets a fresh
/// Store).
fn embed_registry() -> &'static Mutex<HashMap<String, WeakStore>> {
    static REG: OnceLock<Mutex<HashMap<String, WeakStore>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

fn registry_key(target: &Target) -> Option<String> {
    match target {
        Target::EmbedMemoryAnonymous | Target::Remote(_) => None,
        Target::EmbedMemoryNamed(name) => Some(format!("mem://{name}")),
        Target::EmbedPersist(path) => Some(format!("file://{}", path.display())),
    }
}

/// Resolve `target` to a `Store`. Anonymous `mem://` always gets a fresh
/// Store; named / persist targets return the cached one when present.
pub(crate) fn resolve_store(target: &Target) -> io::Result<Store> {
    let key = registry_key(target);
    if let Some(k) = &key
        && let Ok(mut r) = embed_registry().lock()
    {
        r.retain(|_, w| w.upgrade().is_some());
        if let Some(store) = r.get(k).and_then(kevy_embedded::WeakStore::upgrade) {
            return Ok(store);
        }
    }
    let store = match target {
        Target::EmbedMemoryAnonymous | Target::EmbedMemoryNamed(_) => {
            Store::open(Config::default())
        }
        Target::EmbedPersist(path) => Store::open(Config::default().with_persist(path)),
        Target::Remote(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "resolve_store called on a Remote target",
            ));
        }
    }?;
    if let Some(k) = key
        && let Ok(mut r) = embed_registry().lock()
    {
        r.insert(k, store.downgrade());
    }
    Ok(store)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mem_url() {
        assert!(matches!(
            parse_url("mem://").unwrap(),
            Target::EmbedMemoryAnonymous
        ));
        match parse_url("mem://my-bus").unwrap() {
            Target::EmbedMemoryNamed(n) => assert_eq!(n, "my-bus"),
            other => panic!("expected EmbedMemoryNamed, got {other:?}"),
        }
    }

    #[test]
    fn parse_file_url() {
        match parse_url("file:///var/lib/myapp").unwrap() {
            Target::EmbedPersist(p) => assert_eq!(p, PathBuf::from("/var/lib/myapp")),
            _ => panic!("wrong variant"),
        }
        match parse_url("file://./data").unwrap() {
            Target::EmbedPersist(p) => assert_eq!(p, PathBuf::from("./data")),
            _ => panic!("wrong variant"),
        }
        assert!(parse_url("file://").is_err());
    }

    #[test]
    fn parse_remote_urls_delegate() {
        for url in ["kevy://h:6379", "redis://h:6379/0", "tcp://h:6379"] {
            match parse_url(url).unwrap() {
                Target::Remote(u) => assert_eq!(u, url),
                _ => panic!("wrong variant"),
            }
        }
    }

    #[test]
    fn parse_tls_rejected() {
        assert_eq!(
            parse_url("rediss://h:6379").unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
    }

    #[test]
    fn parse_unknown_scheme_rejected() {
        assert_eq!(
            parse_url("memcached://h:11211").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
