//! `[secure]`: this node's static key for the encrypted links, and the
//! encrypted client port. Nothing is encrypted until a link's own
//! `secure = true` is set (`[cluster]`, `[replication]`) or `listen_port`
//! is given.

use std::path::PathBuf;

use crate::apply::{schema_err, value_as_list, value_as_string};
use crate::parse::Item;
use crate::schema::{Config, ConfigError};

/// Where this node's X25519 private key is kept.
///
/// ```
/// let cfg = kevy_config::Config::from_toml_str("[secure]\nprivate_key_file = \"/etc/kevy/node.key\"\n", None).unwrap();
/// assert_eq!(cfg.secure.private_key_file.unwrap().to_str(), Some("/etc/kevy/node.key"));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SecureSection {
    /// A file holding the private key as 64 hex characters, written by
    /// `kevy keygen`. `None` (default): no key, so no link may be secure.
    ///
    /// ```
    /// assert_eq!(kevy_config::Config::default().secure.private_key_file, None);
    /// ```
    pub private_key_file: Option<PathBuf>,
    /// The port for encrypted client connections, on the same address as
    /// the plaintext one. `0` (default): no encrypted client port.
    ///
    /// ```
    /// let cfg = kevy_config::Config::from_toml_str("[secure]\nlisten_port = 6404\n", None).unwrap();
    /// assert_eq!(cfg.secure.listen_port, 6404);
    /// assert_eq!(kevy_config::Config::default().secure.listen_port, 0);
    /// ```
    pub listen_port: u16,
    /// Client public keys allowed on the encrypted port. Empty (default):
    /// any client may connect, and the connection is still encrypted.
    ///
    /// ```
    /// let src = format!("[secure]\nclient_keys = [\"{}\"]\n", "ab".repeat(32));
    /// let cfg = kevy_config::Config::from_toml_str(&src, None).unwrap();
    /// assert_eq!(cfg.secure.client_keys, vec![[0xab; 32]]);
    /// ```
    pub client_keys: Vec<[u8; 32]>,
}

/// A 32-byte public key from its 64-character hex form.
///
/// ```
/// let k = kevy_config::key_from_hex(&"ab".repeat(32)).unwrap();
/// assert_eq!(k, [0xab; 32]);
/// assert!(kevy_config::key_from_hex("abc").is_err());
/// ```
pub fn key_from_hex(s: &str) -> Result<[u8; 32], String> {
    let s = s.trim();
    if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("a key is 64 hex characters, got {:?}", s));
    }
    let mut k = [0u8; 32];
    for (i, b) in k.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).map_err(|e| e.to_string())?;
    }
    Ok(k)
}

/// The 64-character lower-case hex form of a key.
///
/// ```
/// assert_eq!(kevy_config::key_to_hex(&[0xab; 32]), "ab".repeat(32));
/// ```
pub fn key_to_hex(k: &[u8; 32]) -> String {
    k.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn key_item(item: &Item) -> Result<[u8; 32], ConfigError> {
    key_from_hex(&value_as_string(item)?).map_err(|e| schema_err(item, e))
}

/// `["n1=<hex>", "n2=<hex>"]` (or one comma-separated string) as
/// `(node_id, key)` pairs.
pub(crate) fn peer_keys_item(item: &Item) -> Result<Vec<(String, [u8; 32])>, ConfigError> {
    value_as_list(item)?
        .iter()
        .map(|t| {
            let (id, key) = t
                .split_once('=')
                .ok_or_else(|| schema_err(item, format!("{t:?} is not id=key")))?;
            let key = key_from_hex(key).map_err(|e| schema_err(item, e))?;
            Ok((id.trim().to_string(), key))
        })
        .collect()
}

/// A list of bare hex keys.
pub(crate) fn keys_item(item: &Item) -> Result<Vec<[u8; 32]>, ConfigError> {
    value_as_list(item)?.iter().map(|k| key_from_hex(k).map_err(|e| schema_err(item, e))).collect()
}

impl Config {
    pub(crate) fn apply_secure(&mut self, item: &Item) -> Result<(), ConfigError> {
        match item.key.as_str() {
            "private_key_file" => {
                self.secure.private_key_file = Some(PathBuf::from(value_as_string(item)?))
            }
            "listen_port" => self.secure.listen_port = crate::apply::value_as_u16(item)?,
            "client_keys" => self.secure.client_keys = keys_item(item)?,
            k => return Err(schema_err(item, format!("unknown [secure] key: {k}"))),
        }
        Ok(())
    }
}
