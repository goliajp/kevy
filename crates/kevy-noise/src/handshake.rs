//! The IK handshake: `<- s ... -> e, es, s, ss  <- e, ee, se`.

use kevy_crypto::{ct_eq, x25519};

use crate::symmetric::SymmetricState;
use crate::{Error, Transport};

const PROTOCOL: &[u8] = b"Noise_IK_25519_ChaChaPoly_BLAKE2s";
const DHLEN: usize = 32;
const TAG: usize = 16;

/// An X25519 key pair.
///
/// ```
/// use kevy_noise::Keypair;
///
/// let kp = Keypair::from_secret([7; 32]);
/// assert_eq!(kp.public(), Keypair::from_secret([7; 32]).public());
/// ```
#[derive(Clone)]
pub struct Keypair {
    secret: [u8; 32],
    public: [u8; 32],
}

// the secret never reaches a log line
impl core::fmt::Debug for Keypair {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Keypair").field("public", &self.public).finish_non_exhaustive()
    }
}

impl Keypair {
    /// Derive the public key of `secret`.
    ///
    /// ```
    /// let kp = kevy_noise::Keypair::from_secret([1; 32]);
    /// assert_ne!(kp.public(), [0; 32]);
    /// ```
    pub fn from_secret(secret: [u8; 32]) -> Self {
        Keypair { public: x25519::x25519(&secret, &x25519::BASEPOINT), secret }
    }

    /// The public key, X25519(secret, 9): what the peer is given.
    ///
    /// ```
    /// let kp = kevy_noise::Keypair::from_secret([1; 32]);
    /// assert_ne!(kp.public(), [0; 32]);
    /// ```
    pub fn public(&self) -> [u8; 32] {
        self.public
    }
}

/// X25519, refusing an all-zero result (a peer that sent a point of small
/// order).
fn dh(secret: &[u8; 32], public: &[u8; 32]) -> Result<[u8; 32], Error> {
    let shared = x25519::x25519(secret, public);
    if ct_eq(&shared, &[0; 32]) {
        return Err(Error::LowOrderPoint);
    }
    Ok(shared)
}

fn take<'a>(msg: &mut &'a [u8], n: usize) -> Result<&'a [u8], Error> {
    if msg.len() < n {
        return Err(Error::Truncated);
    }
    let (head, rest) = msg.split_at(n);
    *msg = rest;
    Ok(head)
}

fn key32(b: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 32];
    k.copy_from_slice(b);
    k
}

/// The initiator after sending the first message, waiting for the reply.
///
/// ```
/// use kevy_noise::{Initiator, Keypair};
///
/// let server = Keypair::from_secret([1; 32]);
/// let (first, _waiting) =
///     Initiator::start(&Keypair::from_secret([2; 32]), &server.public(), Keypair::from_secret([3; 32]), b"", b"")
///         .unwrap();
/// assert_eq!(first.len(), 32 + 48 + 16);
/// ```
pub struct Initiator {
    ss: SymmetricState,
    s: Keypair,
    e: Keypair,
}

impl Initiator {
    /// Write the first handshake message to a responder whose static
    /// public key is `remote_static`. `ephemeral` must be fresh for every
    /// handshake.
    ///
    /// ```
    /// use kevy_noise::{Initiator, Keypair, Responder};
    ///
    /// let server = Keypair::from_secret([1; 32]);
    /// let client = Keypair::from_secret([2; 32]);
    /// let (msg1, init) =
    ///     Initiator::start(&client, &server.public(), Keypair::from_secret([3; 32]), b"p", b"hi").unwrap();
    /// let (payload, resp) = Responder::accept(&server, Keypair::from_secret([4; 32]), b"p", &msg1).unwrap();
    /// assert_eq!(payload, b"hi");
    /// assert_eq!(resp.remote_static(), client.public());
    /// let (msg2, mut server_t) = resp.finish(b"").unwrap();
    /// let (_, mut client_t) = init.finish(&msg2).unwrap();
    /// let sealed = client_t.seal(b"ping").unwrap();
    /// assert_eq!(server_t.open(&sealed).unwrap(), b"ping");
    /// ```
    pub fn start(
        local: &Keypair,
        remote_static: &[u8; 32],
        ephemeral: Keypair,
        prologue: &[u8],
        payload: &[u8],
    ) -> Result<(Vec<u8>, Initiator), Error> {
        let mut ss = SymmetricState::new(PROTOCOL);
        ss.mix_hash(prologue);
        ss.mix_hash(remote_static);
        let mut msg = ephemeral.public.to_vec();
        ss.mix_hash(&ephemeral.public);
        ss.mix_key(&dh(&ephemeral.secret, remote_static)?);
        msg.extend(ss.encrypt_and_hash(&local.public)?);
        ss.mix_key(&dh(&local.secret, remote_static)?);
        msg.extend(ss.encrypt_and_hash(payload)?);
        Ok((msg, Initiator { ss, s: local.clone(), e: ephemeral }))
    }

    /// Read the responder's reply: its payload, and the transport.
    ///
    /// ```
    /// use kevy_noise::{Error, Initiator, Keypair};
    ///
    /// let server = Keypair::from_secret([1; 32]);
    /// let (_, init) =
    ///     Initiator::start(&Keypair::from_secret([2; 32]), &server.public(), Keypair::from_secret([3; 32]), b"", b"")
    ///         .unwrap();
    /// assert_eq!(init.finish(&[0; 10]).err(), Some(Error::Truncated));
    /// ```
    pub fn finish(mut self, reply: &[u8]) -> Result<(Vec<u8>, Transport), Error> {
        let mut rest = reply;
        let re = key32(take(&mut rest, DHLEN)?);
        self.ss.mix_hash(&re);
        self.ss.mix_key(&dh(&self.e.secret, &re)?);
        self.ss.mix_key(&dh(&self.s.secret, &re)?);
        let payload = self.ss.decrypt_and_hash(rest)?;
        let (send, recv, hash) = self.ss.split();
        Ok((payload, Transport::new(send, recv, hash)))
    }
}

/// The responder after reading the first message, before replying.
///
/// ```
/// use kevy_noise::{Initiator, Keypair, Responder};
///
/// let (server, client) = (Keypair::from_secret([1; 32]), Keypair::from_secret([2; 32]));
/// let (m1, _) = Initiator::start(&client, &server.public(), Keypair::from_secret([3; 32]), b"", b"").unwrap();
/// let (_, responder) = Responder::accept(&server, Keypair::from_secret([4; 32]), b"", &m1).unwrap();
/// let (reply, _) = responder.finish(b"").unwrap();
/// assert_eq!(reply.len(), 32 + 16);
/// ```
pub struct Responder {
    ss: SymmetricState,
    e: Keypair,
    re: [u8; 32],
    rs: [u8; 32],
}

impl Responder {
    /// Read an initiator's first message: its payload, and the state to
    /// reply from. `ephemeral` must be fresh for every handshake.
    ///
    /// ```
    /// use kevy_noise::{Error, Keypair, Responder};
    ///
    /// let server = Keypair::from_secret([1; 32]);
    /// let r = Responder::accept(&server, Keypair::from_secret([4; 32]), b"", &[0; 95]);
    /// assert!(matches!(r, Err(Error::Truncated | Error::Decrypt | Error::LowOrderPoint)));
    /// ```
    pub fn accept(
        local: &Keypair,
        ephemeral: Keypair,
        prologue: &[u8],
        first: &[u8],
    ) -> Result<(Vec<u8>, Responder), Error> {
        let mut ss = SymmetricState::new(PROTOCOL);
        ss.mix_hash(prologue);
        ss.mix_hash(&local.public);
        let mut rest = first;
        let re = key32(take(&mut rest, DHLEN)?);
        ss.mix_hash(&re);
        ss.mix_key(&dh(&local.secret, &re)?);
        let rs = key32(&ss.decrypt_and_hash(take(&mut rest, DHLEN + TAG)?)?);
        ss.mix_key(&dh(&local.secret, &rs)?);
        let payload = ss.decrypt_and_hash(rest)?;
        Ok((payload, Responder { ss, e: ephemeral, re, rs }))
    }

    /// The initiator's static public key, authenticated by the handshake so
    /// far: decide here whether to let it in.
    ///
    /// ```
    /// use kevy_noise::{Initiator, Keypair, Responder};
    ///
    /// let (server, client) = (Keypair::from_secret([1; 32]), Keypair::from_secret([2; 32]));
    /// let (m1, _) = Initiator::start(&client, &server.public(), Keypair::from_secret([3; 32]), b"", b"").unwrap();
    /// let (_, r) = Responder::accept(&server, Keypair::from_secret([4; 32]), b"", &m1).unwrap();
    /// assert_eq!(r.remote_static(), client.public());
    /// ```
    pub fn remote_static(&self) -> [u8; 32] {
        self.rs
    }

    /// Write the reply carrying `payload`, and return the transport.
    ///
    /// ```
    /// use kevy_noise::{Initiator, Keypair, Responder};
    ///
    /// let (server, client) = (Keypair::from_secret([1; 32]), Keypair::from_secret([2; 32]));
    /// let (m1, init) = Initiator::start(&client, &server.public(), Keypair::from_secret([3; 32]), b"", b"").unwrap();
    /// let (_, r) = Responder::accept(&server, Keypair::from_secret([4; 32]), b"", &m1).unwrap();
    /// let (m2, _) = r.finish(b"welcome").unwrap();
    /// assert_eq!(init.finish(&m2).unwrap().0, b"welcome");
    /// ```
    pub fn finish(mut self, payload: &[u8]) -> Result<(Vec<u8>, Transport), Error> {
        let mut msg = self.e.public.to_vec();
        self.ss.mix_hash(&self.e.public);
        self.ss.mix_key(&dh(&self.e.secret, &self.re)?);
        self.ss.mix_key(&dh(&self.e.secret, &self.rs)?);
        msg.extend(self.ss.encrypt_and_hash(payload)?);
        let (recv, send, hash) = self.ss.split();
        Ok((msg, Transport::new(send, recv, hash)))
    }
}
