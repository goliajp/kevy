//! Keys for encrypting an embed's replication links with Noise IK. Both
//! directions stay plaintext unless one of these is configured.

use crate::config::Config;

pub use kevy_noise::Keypair;

/// One end's key pair, and the public keys of the other ends it accepts.
///
/// ```
/// use kevy_embedded::{Keypair, LinkKeys};
///
/// let keys = LinkKeys { local: Keypair::from_secret([1; 32]), peers: vec![[2; 32]] };
/// assert_eq!(keys.peers.len(), 1);
/// ```
#[derive(Debug, Clone)]
pub struct LinkKeys {
    /// This store's static key pair.
    ///
    /// ```
    /// let keys = kevy_embedded::LinkKeys { local: kevy_embedded::Keypair::from_secret([1; 32]), peers: vec![] };
    /// assert_ne!(keys.local.public(), [0; 32]);
    /// ```
    pub local: Keypair,
    /// On a replica, the primary keys it trusts, tried in turn until one
    /// answers; it needs at least one. On a writer, the replica keys it
    /// accepts; empty accepts any replica, still encrypted.
    ///
    /// ```
    /// let primary = kevy_embedded::Keypair::from_secret([2; 32]);
    /// let keys = kevy_embedded::LinkKeys {
    ///     local: kevy_embedded::Keypair::from_secret([1; 32]),
    ///     peers: vec![primary.public()],
    /// };
    /// assert_eq!(keys.peers[0], primary.public());
    /// ```
    pub peers: Vec<[u8; 32]>,
}

/// The keys for each replication direction; `None` keeps it plaintext.
#[derive(Debug, Clone, Default)]
pub(crate) struct LinkSecurity {
    pub(crate) replica: Option<LinkKeys>,
    pub(crate) writer: Option<LinkKeys>,
}

impl Config {
    /// Encrypt the link to the replication upstream: the replica presents
    /// `keys.local` and connects only to a primary holding one of
    /// `keys.peers`. Opening fails when `keys.peers` is empty.
    ///
    /// ```
    /// use kevy_embedded::{Config, Keypair, LinkKeys, Store};
    ///
    /// let primary = Keypair::from_secret([2; 32]);
    /// let replica = |peers| {
    ///     Config::default()
    ///         .without_aof()
    ///         .with_replica_upstream("127.0.0.1:16004")
    ///         .with_replica_security(LinkKeys { local: Keypair::from_secret([1; 32]), peers })
    /// };
    /// assert!(Store::open(replica(vec![primary.public()]))?.is_replica());
    /// // a secure replica that trusts no primary does not open
    /// assert!(Store::open(replica(vec![])).is_err());
    /// # Ok::<(), kevy_embedded::KevyError>(())
    /// ```
    #[must_use]
    pub fn with_replica_security(mut self, keys: LinkKeys) -> Self {
        self.link_security.replica = Some(keys);
        self
    }

    /// Encrypt the embed-writer listener: every replica must complete a
    /// Noise handshake against `keys.local`, and when `keys.peers` is not
    /// empty, present one of those keys.
    ///
    /// ```
    /// use kevy_embedded::{Config, Keypair, LinkKeys, Store};
    ///
    /// let writer = Store::open(
    ///     Config::default()
    ///         .with_embed_writer("127.0.0.1:0")
    ///         .with_writer_security(LinkKeys { local: Keypair::from_secret([1; 32]), peers: vec![] }),
    /// )?;
    /// assert!(writer.writer_addr().is_some());
    /// # Ok::<(), kevy_embedded::KevyError>(())
    /// ```
    #[must_use]
    pub fn with_writer_security(mut self, keys: LinkKeys) -> Self {
        self.link_security.writer = Some(keys);
        self
    }
}
