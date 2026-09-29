//! How a replica opens its link: [`ConnectOptions`] and
//! [`ReplicaClient::connect_with`].

use std::net::ToSocketAddrs;
use std::time::Duration;

use crate::feed::FeedPosition;
use crate::replica::ReplicaClient;
use crate::replica_error::ReplicaError;
#[cfg(feature = "secure")]
use crate::replica_secure::ReplicaSecurity;

/// Everything a replica presents when it connects, beyond the address.
/// The defaults are a fresh replica: generation 0 (no continuity claim),
/// offset 0, a 5 s connect timeout, a plaintext link.
///
/// ```
/// use std::time::Duration;
/// use kevy_replicate::feed::FeedPosition;
/// use kevy_replicate::replica::ConnectOptions;
///
/// let opts = ConnectOptions::new("replica-a")
///     .with_from(FeedPosition::new(7, 42))
///     .with_timeout(Duration::from_secs(1));
/// assert_eq!(opts.from, FeedPosition::new(7, 42));
/// ```
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ConnectOptions {
    /// The replica's identifier, operator-set; the primary keys its slot
    /// by it.
    pub replica_id: String,
    /// Where to resume: the feed generation this replica's data reflects
    /// (`0` = unknown or fresh) and the offset within it. The primary
    /// serves the offset's continuity only when the generations match;
    /// otherwise it ships a snapshot.
    pub from: FeedPosition,
    /// How long the connect and the handshake may take.
    pub timeout: Duration,
    /// `Some` to open a Noise IK link instead of a plaintext one.
    #[cfg(feature = "secure")]
    pub security: Option<ReplicaSecurity>,
}

impl ConnectOptions {
    /// A fresh replica named `replica_id`, with the defaults above.
    ///
    /// ```
    /// let opts = kevy_replicate::replica::ConnectOptions::new("r1");
    /// assert_eq!(opts.from, kevy_replicate::feed::FeedPosition::default());
    /// ```
    pub fn new(replica_id: impl Into<String>) -> Self {
        ConnectOptions {
            replica_id: replica_id.into(),
            from: FeedPosition::default(),
            timeout: Duration::from_secs(5),
            #[cfg(feature = "secure")]
            security: None,
        }
    }

    /// Set [`ConnectOptions::from`].
    ///
    /// ```
    /// use kevy_replicate::feed::FeedPosition;
    ///
    /// let opts = kevy_replicate::replica::ConnectOptions::new("r1").with_from(FeedPosition::new(3, 9));
    /// assert_eq!(opts.from, FeedPosition::new(3, 9));
    /// ```
    #[must_use]
    pub fn with_from(mut self, from: FeedPosition) -> Self {
        self.from = from;
        self
    }

    /// Set [`ConnectOptions::timeout`].
    ///
    /// ```
    /// use std::time::Duration;
    /// let opts = kevy_replicate::replica::ConnectOptions::new("r1").with_timeout(Duration::from_secs(1));
    /// assert_eq!(opts.timeout, Duration::from_secs(1));
    /// ```
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Open the link under Noise IK with `security`.
    ///
    /// ```
    /// use kevy_replicate::replica::{ConnectOptions, ReplicaSecurity};
    ///
    /// let sec = ReplicaSecurity::new(kevy_noise::Keypair::from_secret([2; 32]), [9; 32]);
    /// let opts = ConnectOptions::new("r1").with_security(sec);
    /// assert!(opts.security.is_some());
    /// ```
    #[cfg(feature = "secure")]
    #[must_use]
    pub fn with_security(mut self, security: ReplicaSecurity) -> Self {
        self.security = Some(security);
        self
    }
}

impl ReplicaClient {
    /// Connect to `addr` as `opts` describes, send `REPLICATE FROM
    /// <generation> <from_offset> ID <replica_id>`, read the `+ACK <gen>
    /// <offset>` reply, and return a ready-to-iterate client. A secure
    /// link authenticates the primary by `opts.security`'s key before the
    /// handshake, and everything after it is encrypted.
    ///
    /// ```no_run
    /// use kevy_replicate::replica::{ConnectOptions, ReplicaClient};
    ///
    /// let opts = ConnectOptions::new("replica-a").with_from(kevy_replicate::feed::FeedPosition::new(7, 42));
    /// let client = ReplicaClient::connect_with("127.0.0.1:16004", &opts)?;
    /// assert_eq!(client.expected_offset(), 42);
    /// # Ok::<(), kevy_replicate::replica::ReplicaError>(())
    /// ```
    pub fn connect_with<A: ToSocketAddrs>(
        addr: A,
        opts: &ConnectOptions,
    ) -> Result<Self, ReplicaError> {
        #[cfg(feature = "secure")]
        if let Some(security) = &opts.security {
            return Self::connect_noise(addr, opts, security);
        }
        Self::connect_plain(addr, opts)
    }
}
