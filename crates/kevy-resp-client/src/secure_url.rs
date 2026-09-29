//! `kevys://` URLs and client key files.

use std::io;
use std::path::{Path, PathBuf};

use kevy_noise::Keypair;

/// The key pieces of a `kevys://` URL: the server's public key, and the
/// file holding this client's key pair, if any.
///
/// ```
/// let u = kevy_resp_client::SecureUrl::parse(&format!("kevys://h:6404?server_key={}", "ab".repeat(32)))?;
/// assert_eq!((u.host.as_str(), u.port, u.server_key), ("h", 6404, [0xab; 32]));
/// assert_eq!(u.client_key_file, None);
/// # Ok::<(), std::io::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SecureUrl {
    /// Hostname or IP literal.
    ///
    /// ```
    /// let u = kevy_resp_client::SecureUrl::parse(&format!("kevys://db.internal?server_key={}", "ab".repeat(32)))?;
    /// assert_eq!(u.host, "db.internal");
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub host: String,
    /// TCP port of the encrypted client port; 6379 when omitted.
    ///
    /// ```
    /// let u = kevy_resp_client::SecureUrl::parse(&format!("kevys://h?server_key={}", "ab".repeat(32)))?;
    /// assert_eq!(u.port, 6379);
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub port: u16,
    /// Optional db index from a `/N` path component.
    ///
    /// ```
    /// let u = kevy_resp_client::SecureUrl::parse(&format!("kevys://h:1/0?server_key={}", "ab".repeat(32)))?;
    /// assert_eq!(u.db, Some(0));
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub db: Option<u32>,
    /// The server's public key, from `server_key=` (64 hex characters).
    ///
    /// ```
    /// assert!(kevy_resp_client::SecureUrl::parse("kevys://h:1").is_err()); // required
    /// ```
    pub server_key: [u8; 32],
    /// This client's private key file, from `client_key_file=`, in the
    /// format `kevy keygen` writes.
    ///
    /// ```
    /// let u = kevy_resp_client::SecureUrl::parse(&format!(
    ///     "kevys://h:1?server_key={}&client_key_file=/etc/app/kevy.key",
    ///     "ab".repeat(32)
    /// ))?;
    /// assert_eq!(u.client_key_file.as_deref(), Some(std::path::Path::new("/etc/app/kevy.key")));
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub client_key_file: Option<PathBuf>,
}

impl SecureUrl {
    /// Parse `kevys://host[:port][/db]?server_key=<hex>[&client_key_file=<path>]`.
    ///
    /// ```
    /// use kevy_resp_client::SecureUrl;
    /// assert!(SecureUrl::parse("kevy://h:1").is_err()); // not a kevys:// URL
    /// assert!(SecureUrl::parse("kevys://h:1?server_key=abc").is_err()); // short key
    /// assert!(SecureUrl::parse(&format!("kevys://h:1?server_key={}&x=1", "ab".repeat(32))).is_err());
    /// ```
    pub fn parse(url: &str) -> io::Result<SecureUrl> {
        let invalid = |m: String| io::Error::new(io::ErrorKind::InvalidInput, m);
        let rest = url
            .strip_prefix("kevys://")
            .ok_or_else(|| invalid(format!("not a kevys:// URL: {url}")))?;
        let (base, query) = rest.split_once('?').unwrap_or((rest, ""));
        let plain = crate::ParsedUrl::parse(&format!("kevy://{base}"))?;
        let (mut server_key, mut client_key_file) = (None, None);
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            match pair.split_once('=') {
                Some(("server_key", v)) => server_key = Some(key_from_hex(v)?),
                Some(("client_key_file", v)) if !v.is_empty() => {
                    client_key_file = Some(PathBuf::from(v))
                }
                _ => return Err(invalid(format!("unknown kevys:// parameter: {pair}"))),
            }
        }
        let server_key = server_key.ok_or_else(|| {
            invalid("kevys:// needs server_key=<the server's public key>".to_string())
        })?;
        Ok(SecureUrl {
            host: plain.host,
            port: plain.port,
            db: plain.db,
            server_key,
            client_key_file,
        })
    }
}

/// Read a key pair from a file holding the private key as 64 hex
/// characters, as `kevy keygen` writes it.
///
/// ```no_run
/// let me = kevy_resp_client::load_client_key(std::path::Path::new("/etc/app/kevy.key"))?;
/// println!("{:02x?}", me.public());
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn load_client_key(path: &Path) -> io::Result<Keypair> {
    let text = std::fs::read_to_string(path)?;
    Ok(Keypair::from_secret(key_from_hex(&text)?))
}

fn key_from_hex(s: &str) -> io::Result<[u8; 32]> {
    let s = s.trim();
    let mut k = [0u8; 32];
    let ok = s.len() == 64
        && s.is_ascii()
        && k.iter_mut()
            .enumerate()
            .all(|(i, b)| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).map(|v| *b = v).is_ok());
    if ok {
        Ok(k)
    } else {
        Err(io::Error::new(io::ErrorKind::InvalidInput, "a key is 64 hex characters"))
    }
}
